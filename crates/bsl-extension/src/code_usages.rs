// Обратный индекс использований объектов метаданных 1С В КОДЕ (`.bsl`).
//
// В отличие от `data_links` (декларативные ссылки из XML-метаданных) и
// `proc_call_graph` (граф вызовов процедур), эта таблица — КОД-производная:
// «где в коде упоминается объект X». Лёгкий source-aware regex-слой, порт
// логики rlm-tools-bsl (`_extract_code_usages` / `_scan_module`).
//
// Ловит обращения к объектам метаданных:
//   * `manager`  — обращение к менеджеру коллекции `Документы.X` / `Documents.X`
//     (ищется в РЕАЛЬНОМ коде: строковые литералы и комментарии вырезаны,
//     включая многострочные литералы);
//   * `ref_type` — тип-ссылка в строковом литерале: `"ДокументСсылка.X"` (RU)
//     либо `"DocumentRef.X"` (EN);
//   * `query`    — путь метаданных в строковом литерале (текст запроса):
//     `Документ.X` и `Документ.X.Товары` (3-й сегмент = имя ТЧ → `member_path`).
//
// Ветки `template_call*`, `common_picture`, `style_item`,
// `common_module_via_literal` разбирает [`extract_extra_usages`]: они
// пересекаются с базовым разбором по входу (тот же вызов `ПолучитьМакет`), и
// один вызов не должен попасть в один набор дважды. Индексатор складывает оба
// набора через [`extract_code_usages_for_index`], потребители фильтруют по
// `usage_kind`.
//   * `template_call`       — `ПолучитьМакет("Имя")`: имя макета в литерале
//     (аргумент — ОДИН строковый литерал). Владелец разрешается при индексации
//     по пути модуля: `object_ref = "<Владелец>.Template.<Имя>"`. Без владельца
//     (общий модуль) строка остаётся парой `object_ref = "Template"`,
//     `member_path = Some(имя макета)`.
//   * `common_template_call` — `ПолучитьОбщийМакет("Имя")`: общий макет
//     разрешается всегда, `object_ref = "CommonTemplate.<Имя>"`.
//   * `template_call_via_manager` — метод-форма `Справочники.X.ПолучитьМакет("Y")`
//     (и EN-коллекции `Catalogs.X`): приёмник — менеджер коллекции из
//     [`manager_map`], владелец макета известен сразу, без резолва по файлу:
//     `object_ref = "<canonical>.<ВторойИдент>"` (например `Catalog.X`),
//     `member_path = Some("Y")`.
//   * `template_call_via_receiver` — метод-форма с приёмником-выражением
//     (`ПолучитьМакетОбъекта(Ссылка).ПолучитьМакет("Y")`) или нераспознанным
//     идентификатором: из приёмника владельца не вывести, и при известном
//     владельце модуля макет приписывается ему — `object_ref =
//     "<Владелец>.Template.<Имя>"` (вид остаётся счётчиком таких мест);
//     без владельца (общий модуль) — пара `object_ref = "Template"`.
//   * `template_call_non_literal` — вызов макета с НЕРАСПОЗНАННОЙ целью
//     (`ПолучитьМакет(Выражение)`, склейка, второй аргумент): имя макета не
//     записано, `object_ref` = владелец модуля (или менеджер-приёмник),
//     `member_path = None`. Маркер означает «у этого владельца макет берут по
//     имени, собранному в рантайме»;
//   * `common_picture` — обращение к общей библиотеке картинок в коде
//     (`БиблиотекаКартинок.X` / `PictureLibrary.X`) и ссылка в XML форм
//     (`CommonPicture.X`): `object_ref = "CommonPicture.X"`.
//   * `style_item` — обращение к элементу стиля в коде (`ЦветаСтиля.X`,
//     `ШрифтыСтиля.X`) и ссылка `style:X` в XML: `object_ref = "StyleItem.X"`.
//   * `common_module_via_literal` — строковый доступ к общему модулю
//     `ОбщийМодуль("Имя")` (EN: `CommonModule("Имя")`): `object_ref =
//     "CommonModule.<Имя>"`. Это обращение к САМОМУ объекту `CommonModule`
//     (в отличие от канала графа, который видит только вызовы
//     `Модуль.Метод()`); цель задана строкой, поэтому без разбора литерала
//     ссылки в графе нет.
//
// Что НЕ ловит (осознанно): доступ через локальные переменные
// (`Док = Документы.X; Док.Товары`) — нужен type inference, вне охвата.
// Также не индексируются формы, перечисленные непокрытыми в описании `bsl_sql`:
// нелитеральный аргумент (переменная/склейка/второй аргумент), `template_call_english`
// (`GetTemplate`/`GetCommonTemplate`), `template_call_multiline` (вызов с
// переносом строки до литерала), `common_module_via_qualified_or_expression`
// (`ОбщийМодуль(Выражение)` и метод-форма `ОбщегоНазначения.ОбщийМодуль("X")`),
// `common_module_calls_via_*`.
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
    /// Имя ТЧ для `query` (3-й сегмент пути), имя макета для `template_call` /
    /// `common_template_call`, иначе `None`.
    pub member_path: Option<String>,
    /// Вид обращения: `manager` | `ref_type` | `query` | `template_call` |
    /// `common_template_call` | `template_call_via_manager` |
    /// `template_call_via_receiver` | `common_module_via_literal`.
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
    MetaForm {
        canonical: "Catalog",
        ru_singular: "Справочник",
        ru_plural: "Справочники",
        en_singular: "Catalog",
        en_plural: "Catalogs",
        reftypes: &["СправочникСсылка", "СправочникОбъект", "СправочникМенеджер"],
    },
    MetaForm {
        canonical: "Document",
        ru_singular: "Документ",
        ru_plural: "Документы",
        en_singular: "Document",
        en_plural: "Documents",
        reftypes: &["ДокументСсылка", "ДокументОбъект", "ДокументМенеджер"],
    },
    MetaForm {
        canonical: "Enum",
        ru_singular: "Перечисление",
        ru_plural: "Перечисления",
        en_singular: "Enum",
        en_plural: "Enums",
        reftypes: &["ПеречислениеСсылка", "ПеречислениеМенеджер"],
    },
    MetaForm {
        canonical: "InformationRegister",
        ru_singular: "РегистрСведений",
        ru_plural: "РегистрыСведений",
        en_singular: "InformationRegister",
        en_plural: "InformationRegisters",
        reftypes: &[
            "РегистрСведенийЗапись",
            "РегистрСведенийКлючЗаписи",
            "РегистрСведенийМенеджер",
            "РегистрСведенийНаборЗаписей",
        ],
    },
    MetaForm {
        canonical: "AccumulationRegister",
        ru_singular: "РегистрНакопления",
        ru_plural: "РегистрыНакопления",
        en_singular: "AccumulationRegister",
        en_plural: "AccumulationRegisters",
        reftypes: &[
            "РегистрНакопленияЗапись",
            "РегистрНакопленияКлючЗаписи",
            "РегистрНакопленияМенеджер",
            "РегистрНакопленияНаборЗаписей",
        ],
    },
    MetaForm {
        canonical: "AccountingRegister",
        ru_singular: "РегистрБухгалтерии",
        ru_plural: "РегистрыБухгалтерии",
        en_singular: "AccountingRegister",
        en_plural: "AccountingRegisters",
        reftypes: &[
            "РегистрБухгалтерииЗапись",
            "РегистрБухгалтерииКлючЗаписи",
            "РегистрБухгалтерииМенеджер",
            "РегистрБухгалтерииНаборЗаписей",
        ],
    },
    MetaForm {
        canonical: "CalculationRegister",
        ru_singular: "РегистрРасчета",
        ru_plural: "РегистрыРасчета",
        en_singular: "CalculationRegister",
        en_plural: "CalculationRegisters",
        reftypes: &[
            "РегистрРасчетаЗапись",
            "РегистрРасчетаКлючЗаписи",
            "РегистрРасчетаМенеджер",
            "РегистрРасчетаНаборЗаписей",
        ],
    },
    MetaForm {
        canonical: "ChartOfCharacteristicTypes",
        ru_singular: "ПланВидовХарактеристик",
        ru_plural: "ПланыВидовХарактеристик",
        en_singular: "ChartOfCharacteristicTypes",
        en_plural: "ChartsOfCharacteristicTypes",
        reftypes: &[
            "ПланВидовХарактеристикСсылка",
            "ПланВидовХарактеристикОбъект",
            "ПланВидовХарактеристикМенеджер",
        ],
    },
    MetaForm {
        canonical: "ChartOfAccounts",
        ru_singular: "ПланСчетов",
        ru_plural: "ПланыСчетов",
        en_singular: "ChartOfAccounts",
        en_plural: "ChartsOfAccounts",
        reftypes: &["ПланСчетовСсылка", "ПланСчетовОбъект", "ПланСчетовМенеджер"],
    },
    MetaForm {
        canonical: "ChartOfCalculationTypes",
        ru_singular: "ПланВидовРасчета",
        ru_plural: "ПланыВидовРасчета",
        en_singular: "ChartOfCalculationTypes",
        en_plural: "ChartsOfCalculationTypes",
        reftypes: &[
            "ПланВидовРасчетаСсылка",
            "ПланВидовРасчетаОбъект",
            "ПланВидовРасчетаМенеджер",
        ],
    },
    MetaForm {
        canonical: "ExchangePlan",
        ru_singular: "ПланОбмена",
        ru_plural: "ПланыОбмена",
        en_singular: "ExchangePlan",
        en_plural: "ExchangePlans",
        reftypes: &["ПланОбменаСсылка", "ПланОбменаОбъект", "ПланОбменаМенеджер"],
    },
    MetaForm {
        canonical: "BusinessProcess",
        ru_singular: "БизнесПроцесс",
        ru_plural: "БизнесПроцессы",
        en_singular: "BusinessProcess",
        en_plural: "BusinessProcesses",
        reftypes: &[
            "БизнесПроцессСсылка",
            "БизнесПроцессОбъект",
            "БизнесПроцессМенеджер",
        ],
    },
    MetaForm {
        canonical: "Task",
        ru_singular: "Задача",
        ru_plural: "Задачи",
        en_singular: "Task",
        en_plural: "Tasks",
        reftypes: &["ЗадачаСсылка", "ЗадачаОбъект", "ЗадачаМенеджер"],
    },
    MetaForm {
        canonical: "Report",
        ru_singular: "Отчет",
        ru_plural: "Отчеты",
        en_singular: "Report",
        en_plural: "Reports",
        reftypes: &[],
    },
    MetaForm {
        canonical: "DataProcessor",
        ru_singular: "Обработка",
        ru_plural: "Обработки",
        en_singular: "DataProcessor",
        en_plural: "DataProcessors",
        reftypes: &[],
    },
    MetaForm {
        canonical: "Constant",
        ru_singular: "Константа",
        ru_plural: "Константы",
        en_singular: "Constant",
        en_plural: "Constants",
        reftypes: &[],
    },
    MetaForm {
        canonical: "DocumentJournal",
        ru_singular: "ЖурналДокументов",
        ru_plural: "ЖурналыДокументов",
        en_singular: "DocumentJournal",
        en_plural: "DocumentJournals",
        reftypes: &["ЖурналДокументовМенеджер", "ЖурналДокументовСписок"],
    },
    MetaForm {
        canonical: "Sequence",
        ru_singular: "Последовательность",
        ru_plural: "Последовательности",
        en_singular: "Sequence",
        en_plural: "Sequences",
        reftypes: &[
            "ПоследовательностьМенеджер",
            "ПоследовательностьНаборЗаписей",
            "ПоследовательностьКлючЗаписи",
            "ПоследовательностьЗапись",
        ],
    },
    MetaForm {
        canonical: "FilterCriterion",
        ru_singular: "КритерийОтбора",
        ru_plural: "КритерииОтбора",
        en_singular: "FilterCriterion",
        en_plural: "FilterCriteria",
        reftypes: &["КритерийОтбораМенеджер", "КритерийОтбораСписок"],
    },
];

/// Пары (форма-обращения-в-коде → имя-папки-метаданных) для резолва менеджер-
/// вызовов `Коллекция.Объект.Метод` → `<Папка>/<Объект>/Ext/ManagerModule.bsl`
/// (Tier D в index_extras). Обе формы обращения — RU (`Справочники`) и EN
/// (`Catalogs`) — ведут в одну папку выгрузки (en_plural). Регистр сохраняем как
/// в выгрузке/коде (SQLite lower() не лоуэркейсит кириллицу — сравнение точное).
pub(crate) fn collection_folder_pairs() -> Vec<(&'static str, &'static str)> {
    let mut v = Vec::with_capacity(META_FORMS.len() * 2);
    for f in META_FORMS {
        v.push((f.ru_plural, f.en_plural));
        v.push((f.en_plural, f.en_plural));
    }
    v
}

/// Суффиксы EN-форм тип-ссылок (`DocumentRef`, `CatalogObject`, …). Сверх-набор:
/// несуществующие комбинации (`CatalogRecordSet`) не навредят — они просто не
/// встретятся в коде.
const EN_REF_SUFFIXES: &[&str] = &[
    "Ref",
    "Object",
    "Manager",
    "List",
    "Selection",
    "RecordSet",
    "RecordKey",
    "Record",
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

/// Глобальные свойства-«библиотеки» платформы, из которых берутся элементы
/// стиля и картинки. Обращение `<Коллекция>.<Имя>` в коде адресует объект
/// метаданных ФИКСИРОВАННОГО типа (в отличие от менеджеров коллекций
/// [`META_FORMS`], где тип выводится из имени коллекции):
///
///   * `БиблиотекаКартинок` (RU) / `PictureLibrary` (EN) и EN-форма
///     метаданного `CommonPicture` → `CommonPicture.<Имя>`;
///   * `ЦветаСтиля` / `StyleColors`, `ШрифтыСтиля` / `StyleFonts`,
///     `РамкиСтиля` / `StyleBorders` → `StyleItem.<Имя>`.
///
/// На проверенной выгрузке (prod3) реально встречаются только RU-формы
/// `БиблиотекаКартинок` и `ЦветаСтиля`/`ШрифтыСтиля`; EN-формы добавлены по
/// имени свойства глобального контекста и безвредны, если не встречаются.
/// Единственная форма с неподтверждённым написанием (`colorsstyle`) убрана
/// (E7): в платформе свойство цвета стиля называется `StyleColors` (совр.
/// справка), написания `ColorsStyle` нет ни в одной из проверяемых выгрузок.
/// Значение — `(канонический префикс объекта, usage_kind)`.
fn global_object_map() -> &'static HashMap<String, (&'static str, &'static str)> {
    static M: OnceLock<HashMap<String, (&'static str, &'static str)>> = OnceLock::new();
    M.get_or_init(|| {
        let mut m = HashMap::new();
        for (form, canonical, kind) in [
            ("библиотекакартинок", "CommonPicture", "common_picture"),
            ("picturelibrary", "CommonPicture", "common_picture"),
            ("commonpicture", "CommonPicture", "common_picture"),
            ("цветастиля", "StyleItem", "style_item"),
            ("stylecolors", "StyleItem", "style_item"),
            ("шрифтыстиля", "StyleItem", "style_item"),
            ("stylefonts", "StyleItem", "style_item"),
            ("рамкистиля", "StyleItem", "style_item"),
            ("styleborders", "StyleItem", "style_item"),
        ] {
            m.insert(form.to_string(), (canonical, kind));
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

/// Канонический префикс типа метаданных по RU/EN singular-форме (регистр неважен).
/// `Документ`/`документ`/`Document`/`document` → `Document`. None — тип неизвестен
/// (вызывающий оставляет имя как есть). Переиспользует [`query_map`].
pub(crate) fn canonical_meta_type(meta_type: &str) -> Option<&'static str> {
    query_map().get(&meta_type.to_lowercase()).copied()
}

/// Нормализует ссылку на объект вида `<Тип>.<Имя>` к каноническому английскому
/// типу: `Документ.РеализацияТоваровУслуг` → `Document.РеализацияТоваровУслуг`.
/// В индексе типы хранятся только по-английски (`Document`/`Catalog`/…), поэтому
/// без нормализации запрос с русским префиксом не находит ничего. Неизвестный или
/// уже канонический тип, как и имя без точки, возвращаются без изменений.
pub(crate) fn normalize_object_ref(name: &str) -> std::borrow::Cow<'_, str> {
    use unicode_normalization::UnicodeNormalization;
    let normalized: String = name.nfc().collect();
    let value = match normalized.split_once('.') {
        Some((t, rest)) => match canonical_meta_type(t) {
            Some(canon) if canon != t => format!("{canon}.{rest}"),
            _ => normalized.clone(),
        },
        None => normalized,
    };
    if value == name {
        std::borrow::Cow::Borrowed(name)
    } else {
        std::borrow::Cow::Owned(value)
    }
}

/// Нормализует русские типы-префиксы внутри строковых литералов произвольного SQL
/// (для `bsl_sql`): `'Документ.X'` → `'Document.X'`, `'Документы.X'` → `'Documents.X'`
/// (singular и plural, обе формы кавычек). В индексе типы только английские, поэтому
/// литерал с русским префиксом ничего не находит. Замена консервативная — только по
/// паттерну `<кавычка><РусскийТип>.`, что отсекает совпадения вне ссылок на объект.
pub(crate) fn normalize_sql_object_refs(sql: &str) -> String {
    static PAIRS: OnceLock<Vec<(String, String)>> = OnceLock::new();
    let pairs = PAIRS.get_or_init(|| {
        let mut v = Vec::new();
        for f in META_FORMS {
            if f.ru_singular != f.en_singular {
                v.push((f.ru_singular.to_string(), f.en_singular.to_string()));
            }
            if f.ru_plural != f.en_plural {
                v.push((f.ru_plural.to_string(), f.en_plural.to_string()));
            }
        }
        v
    });
    let mut out = sql.to_string();
    for (ru, en) in pairs {
        out = out.replace(&format!("'{}.", ru), &format!("'{}.", en));
        out = out.replace(&format!("\"{}.", ru), &format!("\"{}.", en));
    }
    out
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
                m.insert(
                    format!("{}{}", f.en_singular, suf).to_lowercase(),
                    f.canonical,
                );
            }
        }
        m
    })
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Маркер строкового литерала в `code`-строке от `scan_line`: встаёт на место
/// кавычки-открытия. По нему `template_calls` связывает вызов с содержимым
/// литерала (сам литерал вырезан из `code`). Управляющий символ не бывает
/// частью реального BSL-кода, поэтому слова/пары `dotted_pairs` он не ломает.
const STRING_MARKER: char = '\u{0}';

/// Разделить строку кода на «реальный код» (без строк/комментариев) и список
/// фрагментов строковых литералов. Порт `_scan_module` (по одной строке).
/// `in_string` — открыт ли многострочный литерал с предыдущей строки.
/// Возвращает `(code, strings, in_string_after)`. Каждому литералу в `code`
/// отвечает маркер [`STRING_MARKER`] в порядке появления (нумерация маркеров
/// совпадает с нумерацией `strings`), — это позволяет искать `Вызов("литерал")`.
fn scan_line(raw: &str, mut in_string: bool) -> (String, Vec<String>, bool) {
    let chars: Vec<char> = raw.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    let mut code = String::new();
    let mut strings: Vec<String> = Vec::new();
    // Начало текущего строкового сегмента. -1 = вне строки.
    let mut seg_start: isize = if in_string { 0 } else { -1 };
    if in_string {
        // Продолжение многострочного литерала: маркер ставится на его месте и
        // в начале строки, чтобы порядок маркеров и `strings` совпадал.
        code.push(STRING_MARKER);
    }

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
            code.push(STRING_MARKER);
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

fn is_space(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Имена макетов из вызовов `ПолучитьМакет("Имя")` / `ПолучитьОбщийМакет("Имя")`
/// в РЕАЛЬНОМ коде строки (Требование 2 ТЗ C2):
///   * имя — ОДИН строковый литерал-аргумент: `ПолучитьМакет("X" + Y)`,
///     `ПолучитьМакет(Переменная)`, второй аргумент — цели не дают;
///   * ГОЛАЯ форма (без приёмника) и приёмник-сам-объект (`ЭтотОбъект`,
///     `Объект`) — `template_call`: при известном `owner_ref` ссылка сразу
///     разрешается (`<Владелец>.Template.<Имя>`), иначе остаётся парой
///     `object_ref = "Template"` + `member_path`;
///   * `ПолучитьОбщийМакет` — `common_template_call` с разрешённой ссылкой
///     `CommonTemplate.<Имя>`: имя общего макета уникально, владелец не нужен;
///   * МЕТОД-форма — `Справочники.X.ПолучитьМакет("Y")` (EN: `Catalogs.X`):
///     если первый идентификатор приёмника есть в [`manager_map`], владелец
///     известен сразу → `template_call_via_manager` с
///     `object_ref = "<canonical>.<ВторойИдент>"` (резолв по файлу не нужен);
///     иначе (`Переменная.ПолучитьМакет("Y")`,
///     `ПолучитьМакетОбъекта(Ссылка).ПолучитьМакет("Y")`) —
///     `template_call_via_receiver`: при известном `owner_ref` макет
///     приписывается владельцу модуля (допущение: в форме отчёта так берут макет
///     своего объекта), иначе остаётся парой-счётчиком;
///   * имя обязано быть похожим на идентификатор 1С (буквы/цифры/`_`).
///
/// Возвращает разобранные строки обращений.
fn template_calls(
    code: &str,
    strings: &[String],
    line: usize,
    owner_ref: Option<&str>,
) -> Vec<CodeUsage> {
    let chars: Vec<char> = code.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < n {
        if !is_word(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && is_word(chars[i]) {
            i += 1;
        }
        let ident: String = chars[start..i].iter().collect();
        let kind = match ident.to_lowercase().as_str() {
            "получитьмакет" => "template_call",
            "получитьобщиймакет" => "common_template_call",
            _ => continue,
        };
        let common = kind == "common_template_call";
        let method_form = start > 0 && chars[start - 1] == '.';
        // Владелец из менеджер-приёмника: `Справочники.X.ПолучитьМакет` →
        // `Catalog.X`; None — приёмник не менеджер коллекции (или формы нет).
        let manager_owner = if method_form {
            receiver_manager_owner(&chars, start - 1)
        } else {
            None
        };
        let mut j = i;
        while j < n && is_space(chars[j]) {
            j += 1;
        }
        if j >= n || chars[j] != '(' {
            continue;
        }
        j += 1;
        while j < n && is_space(chars[j]) {
            j += 1;
        }
        if j >= n || chars[j] != STRING_MARKER {
            push_non_literal_marker(
                &mut out,
                common,
                manager_owner.as_deref().or(owner_ref),
                line,
            );
            continue; // аргумент не литерал (выражение/переменная/перенос строки)
        }
        // Номер маркера = индекс литерала в `strings` этой строки.
        let ordinal = chars[..j].iter().filter(|c| **c == STRING_MARKER).count();
        j += 1;
        // Только закрывающая скобка сразу: иначе это склейка или 2-й аргумент.
        while j < n && is_space(chars[j]) {
            j += 1;
        }
        if j >= n || chars[j] != ')' {
            push_non_literal_marker(
                &mut out,
                common,
                manager_owner.as_deref().or(owner_ref),
                line,
            );
            continue;
        }
        i = j + 1;
        let name = match strings.get(ordinal) {
            Some(name) if !name.is_empty() && name.chars().all(is_word) => name.clone(),
            _ => {
                push_non_literal_marker(
                    &mut out,
                    common,
                    manager_owner.as_deref().or(owner_ref),
                    line,
                );
                continue;
            }
        };
        // Приёмник, указывающий на сам объект модуля: владелец макета — владелец
        // модуля (`owner_ref`), как и у галой формы.
        let self_receiver = method_form && {
            let chain = receiver_chain(&chars, start - 1);
            chain.len() == 1 && matches!(chain[0].to_lowercase().as_str(), "этотобъект" | "объект")
        };
        let (object_ref, object_ref_key, usage_kind) = if let Some(owner) = manager_owner {
            let key = owner.to_lowercase();
            (owner, key, "template_call_via_manager")
        } else if common {
            // Общий макет: имя уникально в конфигурации — владелец не нужен.
            let resolved = format!("CommonTemplate.{name}");
            let key = resolved.to_lowercase();
            (resolved, key, kind)
        } else if !method_form || self_receiver {
            match owner_ref {
                Some(owner) => {
                    let resolved = format!("{owner}.Template.{name}");
                    let key = resolved.to_lowercase();
                    (resolved, key, kind)
                }
                None => ("Template".to_string(), "template".to_string(), kind),
            }
        } else {
            // Приёмник-выражение или нераспознанный идентификатор: из приёмника
            // владельца не вывести. При известном `owner_ref` макет приписывается
            // владельцу модуля — допущение прежнего канала (в форме отчёта так
            // берут макет СВОЕГО объекта); вид остаётся `template_call_via_receiver`,
            // чтобы число таких мест было видно счётчиком. Без `owner_ref` строка
            // остаётся парой без владельца.
            match owner_ref {
                Some(owner) => {
                    let resolved = format!("{owner}.Template.{name}");
                    let key = resolved.to_lowercase();
                    (resolved, key, "template_call_via_receiver")
                }
                None => (
                    "Template".to_string(),
                    "template".to_string(),
                    "template_call_via_receiver",
                ),
            }
        };
        out.push(CodeUsage {
            object_ref,
            object_ref_key,
            member_path: Some(name),
            usage_kind,
            line,
        });
    }
    out
}

/// Строка-маркер НЕРАСПОЗНАННОГО вызова макета: `ПолучитьМакет(Выражение)`,
/// склейка, второй аргумент или литерал не-идентификатор. Цель вызова
/// неизвестна, но владелец известен (менеджер-приёмник либо путь модуля) —
/// этого достаточно, чтобы в отчёте об объектах без ссылок вынести ЕГО макеты в
/// отдельную корзину вычисляемых ссылок, а не объявлять мёртвыми: имя собирается в
/// рантайме (движок регламентированных отчётов), и приговора тут быть не может.
///
/// Общий макет маркера не даёт: корзина вычисляемых ссылок — про макеты
/// владельца, а у общих макетов владельца нет. Вызов без владельца (общий
/// модуль) маркера тоже не даёт — приписать его некому.
fn push_non_literal_marker(
    out: &mut Vec<CodeUsage>,
    common: bool,
    owner: Option<&str>,
    line: usize,
) {
    if common {
        return;
    }
    let Some(owner) = owner else { return };
    out.push(CodeUsage {
        object_ref: owner.to_string(),
        object_ref_key: owner.to_lowercase(),
        member_path: None,
        usage_kind: "template_call_non_literal",
        line,
    });
}

/// Имя общего модуля из строкового доступа `ОбщийМодуль("Имя")` (EN-форма —
/// `CommonModule("Имя")`) в РЕАЛЬНОМ коде строки (Требование 2 ТЗ, C2).
/// Платформенная функция возвращает объект общего модуля ПО ИМЕНИ, поэтому
/// строковый литерал — ссылка на объект `CommonModule.<Имя>`; в
/// `proc_call_graph` такой ссылки нет (там только вызовы `Модуль.Метод()`), а в
/// `metadata_code_usages` до этого вида не было вовсе.
///
///   * имя — ОДИН строковый литерал и ЕДИНСТВЕННЫЙ аргумент: `ОбщийМодуль(Имя)`,
///     `ОбщийМодуль("X" + Суффикс)`, `ОбщийМодуль("X", Второй)` цели не дают
///     (форма названа в `channels_not_applied` как
///     `common_module_via_qualified_or_expression`);
///   * ГОЛАЯ форма — `ОбщийМодуль("Имя")`/`CommonModule("Имя")`; метод-форма
///     (`Переменная.ОбщийМодуль("X")`) не извлекается: у произвольного объекта
///     может быть СВОЙ метод с таким именем, платформенная функция тут ни при
///     чём (та же строка `channels_not_applied`
///     `common_module_via_qualified_or_expression`);
///   * имя обязано быть похожим на идентификатор 1С (буквы/цифры/`_`).
///
/// EN-форма (`CommonModule("Имя")`) принята симметрично остальным формам этого
/// модуля; в проверенных выгрузках (prod3, ЗУП) она не встречалась — там только
/// RU-форма (проверено сплошным сканом всех `.bsl`).
fn common_module_literals(code: &str, strings: &[String], line: usize) -> Vec<CodeUsage> {
    let chars: Vec<char> = code.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < n {
        if !is_word(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && is_word(chars[i]) {
            i += 1;
        }
        let ident: String = chars[start..i].iter().collect();
        match ident.to_lowercase().as_str() {
            "общиймодуль" | "commonmodule" => {}
            _ => continue,
        }
        // Метод-форма: слева точка — это метод чужого объекта, не функция
        // платформы.
        if start > 0 && chars[start - 1] == '.' {
            continue;
        }
        let mut j = i;
        while j < n && is_space(chars[j]) {
            j += 1;
        }
        if j >= n || chars[j] != '(' {
            continue;
        }
        j += 1;
        while j < n && is_space(chars[j]) {
            j += 1;
        }
        if j >= n || chars[j] != STRING_MARKER {
            continue; // аргумент не литерал (выражение/переменная/перенос строки)
        }
        // Номер маркера = индекс литерала в `strings` этой строки.
        let ordinal = chars[..j].iter().filter(|c| **c == STRING_MARKER).count();
        j += 1;
        // Только закрывающая скобка сразу: иначе это склейка или 2-й аргумент.
        while j < n && is_space(chars[j]) {
            j += 1;
        }
        if j >= n || chars[j] != ')' {
            continue;
        }
        i = j + 1;
        let name = match strings.get(ordinal) {
            Some(name) if !name.is_empty() && name.chars().all(is_word) => name.clone(),
            _ => continue,
        };
        out.push(make_usage(
            "CommonModule",
            &name,
            None,
            "common_module_via_literal",
            line,
        ));
    }
    out
}

/// Владелец макета из менеджер-приёмника метод-вызова: `dot` указывает на `.`
/// перед именем метода; цепочка слева — `Идент(.Идент)*`. Если первый
/// идентификатор цепочки — менеджер коллекции из [`manager_map`] (не менее двух
/// сегментов), владелец известен: `<canonical>.<ВторойИдент>` (например
/// `Справочники.Вакансии` → `Catalog.Вакансии`). Иначе `None` — приёмник не
/// менеджер (переменная/объект), владельца по нему не вывести.
fn receiver_manager_owner(chars: &[char], dot: usize) -> Option<String> {
    let chain = receiver_chain(chars, dot);
    if chain.len() < 2 {
        return None;
    }
    let canon = manager_map().get(&chain[0].to_lowercase())?;
    Some(format!("{}.{}", canon, chain[1]))
}

/// Идентификаторы цепочки `Идент(.Идент)*`, стоящей непосредственно перед
/// точкой на позиции `dot` (`chars[dot] == '.'`), слева направо. Пустой вектор —
/// перед точкой идентификатора нет.
fn receiver_chain(chars: &[char], dot: usize) -> Vec<String> {
    let mut segs: Vec<String> = Vec::new();
    let mut k = dot;
    loop {
        if k == 0 {
            segs.clear();
            break;
        }
        let end = k; // эксклюзивная граница сегмента
        let mut s = end;
        while s > 0 && is_word(chars[s - 1]) {
            s -= 1;
        }
        if s == end {
            break; // нет идентификатора слева от точки — цепочка кончилась
        }
        segs.push(chars[s..end].iter().collect());
        if s > 0 && chars[s - 1] == '.' {
            k = s - 1;
        } else {
            break;
        }
    }
    segs.reverse();
    segs
}

/// Непересекающиеся пары `ident.ident` (аналог regex `(\w+)\.(\w+)` finditer).
/// Третий элемент пары — признак «это НЕ ссылка на объект метаданных» (E4):
/// метод (сразу за вторым идентификатором стоит `(`) или доступ к полю объекта
/// (первый идентификатор предшествуется `.`/словом). Признак применяют только к
/// глобальным «библиотекам» (`БиблиотекаКартинок.X`, `ЦветаСтиля.X`): там
/// `БиблиотекаКартинок.Есть(...)` — метод коллекции, а
/// `Объект.БиблиотекаКартинок.Комментарий` — поле объекта, а не объект-картинка.
fn dotted_pairs(s: &str) -> Vec<(String, String, bool)> {
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
                // Доступ к полю: перед первым идентификатором стоит `.`.
                // Проверки `is_word(chars[st - 1])` здесь НЕТ и она недостижима:
                // цикл выше съедает максимальный прогон словных символов, поэтому
                // в `st` он попадает только на позицию, где слева либо начало
                // строки, либо НЕсловный символ (F3).
                let field_access = st > 0 && chars[st - 1] == '.';
                // Метод: сразу за вторым идентификатором — открывающая скобка.
                let method_call = i < n && chars[i] == '(';
                out.push((id1, id2, field_access || method_call));
                // Перекрытие: продолжаем со ВТОРОГО идентификатора пары, а не за
                // ней. Иначе в `Метаданные.Справочники.X` коллекция стоит вторым
                // сегментом, первая пара отбрасывается, и `Справочники.X` уже не
                // рассматривается (E-11). Прогресс гарантирован: s2 строго растёт.
                i = s2;
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

fn make_usage(
    canonical: &str,
    name: &str,
    member: Option<String>,
    kind: &'static str,
    line: usize,
) -> CodeUsage {
    let object_ref = format!("{}.{}", canonical, name);
    let object_ref_key = object_ref.to_lowercase();
    CodeUsage {
        object_ref,
        object_ref_key,
        member_path: member,
        usage_kind: kind,
        line,
    }
}

/// Виртуальные таблицы языка запросов 1С: третий сегмент пути метаданных вида
/// `<Тип>.<Имя>.<ВиртуальнаяТаблица>` — это НЕ табличная часть объекта, а
/// виртуальная таблица платформы (остатки регистра, срез последних и т.п.).
/// Такое обращение остаётся обращением к самому объекту, поэтому третий сегмент
/// в `member_path` не пишется.
pub const QUERY_VIRTUAL_TABLES: &[&str] = &[
    // Русские имена.
    "Остатки",
    "Обороты",
    "ОстаткиИОбороты",
    "СрезПоследних",
    "СрезПервых",
    "ДвиженияСубконто",
    "ДвиженияССубконто",
    "ОборотыДтКт",
    "Изменения",
    // Английские соответствия (тексты запросов встречаются на обоих языках).
    "Balance",
    "Turnovers",
    "BalanceAndTurnovers",
    "SliceLast",
    "SliceFirst",
    "RecordsWithExtDimensions",
    "DrCrTurnovers",
    "Changes",
];

/// Извлечь обращения к объектам метаданных из тела `.bsl`-модуля.
///
/// Базовые каналы: `manager` (пары «коллекция.объект» в коде), `ref_type` и
/// `query` (пути метаданных в строковых литералах). Каналы по строковым
/// аргументам (`ПолучитьМакет`/`ОбщийМодуль`, «библиотеки» картинок и стиля)
/// живут в [`extract_extra_usages`]; индексатор складывает оба набора через
/// [`extract_code_usages_for_index`].
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
            for (g1, g2, _) in dotted_pairs(&code) {
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

        // NB: каналы по строковому аргументу (голая/метод-формы
        // `ПолучитьМакет`, `ОбщийМодуль("Имя")`, «библиотеки» картинок и стиля)
        // живут в [`extract_extra_usages`]; индексатор собирает оба набора
        // через [`extract_code_usages_for_index`].
    }
    out
}

/// Каноническая ссылка на объект-владелец модуля по пути выгрузки:
/// `base/Catalogs/ВидыДокументовЭДО/Ext/ManagerModule.bsl` → `Catalog.ВидыДокументовЭДО`.
///
/// Модуль формы принадлежит тому же владельцу, что и объект:
/// `.../X/Forms/Ф/Ext/Form/Module.bsl` → `...X`. Берётся БЛИЖАЙШАЯ к файлу пара
/// «папка метаданных + объект» — так же работает и вложенная раскладка расширений
/// (`extensions/Расш/Catalogs/X/...`). `None` — владелец не выводится: у общих
/// модулей, общих форм и прочих объектов, которые макетов не имеют, ссылку
/// собирать не из чего.
pub fn owner_ref_from_path(rel_path: &str) -> Option<String> {
    use unicode_normalization::UnicodeNormalization;
    let parts: Vec<&str> = rel_path.split('/').collect();
    let mut found: Option<String> = None;
    for (i, seg) in parts.iter().enumerate() {
        let low = seg.to_lowercase();
        let hit = META_FORMS
            .iter()
            .find(|m| m.ru_plural.to_lowercase() == low || m.en_plural.to_lowercase() == low);
        if let (Some(m), Some(name)) = (hit, parts.get(i + 1)) {
            found = Some(format!(
                "{}.{}",
                m.canonical,
                name.nfc().collect::<String>()
            ));
        }
    }
    found
}

/// Все обращения к объектам метаданных, которые кладёт в индекс индексатор:
/// базовая ветка разбора ([`extract_code_usages`]) + дополнительные каналы
/// ([`extract_extra_usages`]).
///
/// Оба набора не пересекаются по `usage_kind`, поэтому потребители
/// (`find_references`, `get_object_profile`)
/// видят каждый канал ровно один раз, фильтруя по своему виду обращения.
///
/// `owner_ref` — владелец модуля по пути файла ([`owner_ref_from_path`]): он
/// позволяет записать ссылку на макет объекта сразу разрешённой
/// (`<Владелец>.Template.<Имя>`), не откладывая резолв на сторону потребителя.
pub fn extract_code_usages_for_index(content: &str, owner_ref: Option<&str>) -> Vec<CodeUsage> {
    let mut out = extract_code_usages(content);
    out.extend(extract_extra_usages(content, owner_ref));
    out
}

/// Дополнительные каналы обращений к объектам метаданных (ТЗ C2/D1/F1b):
///
///   * `template_call` — голая форма `ПолучитьМакет("Имя")` и метод-форма с
///     приёмником-самим-объектом (`ЭтотОбъект.ПолучитьМакет`, `Объект.ПолучитьМакет`):
///     владелец макета — владелец модуля, поэтому при известном `owner_ref`
///     ссылка записывается РАЗРЕШЁННОЙ (`<Владелец>.Template.<Имя>`), а без него —
///     парой `object_ref = "Template"` + `member_path`;
///   * `common_template_call` — `ПолучитьОбщийМакет("Имя")`: общий макет
///     конфигурации, ссылка разрешается всегда (`CommonTemplate.<Имя>`);
///   * `template_call_via_manager` — `Справочники.X.ПолучитьМакет("Y")`;
///   * `template_call_via_receiver` — метод-форма с приёмником-выражением
///     (`ПолучитьМакетОбъекта(Ссылка).ПолучитьМакет("Y")`) или нераспознанным
///     идентификатором: из приёмника владельца не вывести, и при известном
///     `owner_ref` макет приписывается владельцу модуля (допущение прежнего
///     канала), а без него строка остаётся счётчиком непокрытой формы;
///   * `template_call_non_literal` — вызов макета с нераспознанной целью
///     (выражение, склейка, второй аргумент): `object_ref` = владелец модуля,
///     имя макета не записано; макеты владельца идут в корзину вычисляемых ссылок;
///   * `common_module_via_literal` — `ОбщийМодуль("Имя")`;
///   * `common_picture` / `style_item` — `БиблиотекаКартинок.X`,
///     `ЦветаСтиля.X`, `ШрифтыСтиля.X` (фильтр «метод/поле»).
///
/// Отдельная функция, а не ветка [`extract_code_usages`]: наборы различаются
/// по виду каналов (здесь — вызовы по строковому аргументу и «библиотеки»),
/// и индексатор складывает их через [`extract_code_usages_for_index`].
pub fn extract_extra_usages(content: &str, owner_ref: Option<&str>) -> Vec<CodeUsage> {
    let mut out: Vec<CodeUsage> = Vec::new();
    let mut in_string = false;

    for (idx, raw) in content.split('\n').enumerate() {
        let lineno = idx + 1;
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let (code, strings, carry) = scan_line(raw, in_string);
        in_string = carry;

        // Глобальные «библиотеки» (`БиблиотекаКартинок.X`, `ЦветаСтиля.X`) —
        // пары `Идент.Идент` с фиксированной целью. Менеджеры коллекций здесь не
        // нужны: их даёт `manager` в базовой ветке.
        if code.contains('.') {
            for (g1, g2, not_a_ref) in dotted_pairs(&code) {
                let low = g1.to_lowercase();
                if manager_map().contains_key(&low) {
                    continue;
                }
                if let Some(&(canon, kind)) = global_object_map().get(&low) {
                    // E4: метод коллекции (`БиблиотекаКартинок.Есть(…)`) и доступ
                    // к полю объекта (`Объект.БиблиотекаКартинок.Комментарий`) —
                    // НЕ ссылки на объекты `CommonPicture`/`StyleItem`.
                    if !not_a_ref {
                        out.push(make_usage(canon, &g2, None, kind, lineno));
                    }
                }
            }
        }

        // template_call / common_template_call / *via_manager / *via_receiver —
        // вызов в реальном коде, имя макета — строковый литерал-аргумент.
        // Владелец берётся из `owner_ref` (путь модуля) для галой формы и
        // приёмника-самого-объекта; у метод-формы с менеджер-приёмником он
        // известен из приёмника; нераспознанный приёмник остаётся счётчиком.
        out.extend(template_calls(&code, &strings, lineno, owner_ref));

        // common_module_via_literal — `ОбщийМодуль("Имя")`: строковый доступ к
        // общему модулю по имени (канал общих модулей, F1b). Имя модуля уже
        // канонично (`CommonModule.<Имя>`) — резолв по файлу не нужен.
        out.extend(common_module_literals(&code, &strings, lineno));
    }
    out
}

/// Извлечь обращения к объектам метаданных из ТЕКСТА ЗАПРОСА (не из тела
/// модуля). В отличие от [`extract_code_usages`] здесь нет разбора литералов и
/// комментариев — на вход приходит уже чистый текст запроса (например, из
/// `<query>` макета схемы компоновки), поэтому пути метаданных ищутся по ВСЕЙ
/// строке, построчно.
///
/// Отличия от `extract_code_usages`:
///   * сопоставление только через [`query_map`] (единственная форма типа —
///     `Документ`/`Document`), менеджерных коллекций в запросах нет;
///   * `usage_kind = "query"`;
///   * третий сегмент пути, совпавший с виртуальной таблицей платформы
///     ([`QUERY_VIRTUAL_TABLES`]), не является членом объекта —
///     `member_path = None`.
pub fn extract_query_usages(text: &str) -> Vec<CodeUsage> {
    let mut out: Vec<CodeUsage> = Vec::new();
    for (idx, raw) in text.split('\n').enumerate() {
        let lineno = idx + 1;
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if !raw.contains('.') {
            continue;
        }
        for (g1, g2, g3) in path_triples(raw) {
            let Some(&canon) = query_map().get(&g1.to_lowercase()) else {
                continue;
            };
            let member = match g3 {
                Some(t) if is_virtual_table(&t) => None,
                other => other,
            };
            out.push(make_usage(canon, &g2, member, "query", lineno));
        }
    }
    out
}

/// Третий сегмент пути — виртуальная таблица платформы (регистр независимо от
/// регистра записи: имена виртуальных таблиц кириллические).
fn is_virtual_table(name: &str) -> bool {
    let low = name.to_lowercase();
    QUERY_VIRTUAL_TABLES.iter().any(|v| v.to_lowercase() == low)
}

/// Извлечь обращения к объектам метаданных из XML-ФАЙЛА выгрузки (описание
/// формы, схема, стиль, объектный XML). Понимает две формы ссылок, которые в
/// коде не встречаются (или встречаются иначе):
///
///   * `CommonPicture.<Имя>` — `<xr:Ref>CommonPicture.X</xr:Ref>` в свойствах
///     форм, `<img src="CommonPicture.X">` и пр. →
///     `object_ref = "CommonPicture.X"`, `usage_kind = "common_picture"`;
///   * `style:<Имя>` — цвет/шрифт/рамка стиля (`<TextColor>style:X</TextColor>`,
///     `<dcscor:value xsi:type="v8ui:Color">style:X</dcscor:value>`) →
///     `object_ref = "StyleItem.X"`, `usage_kind = "style_item"`.
///
/// Разбор лёгкий и построчный: цель — не структура XML, а сами ссылки, поэтому
/// ищем маркер подстрокой. Подавать `ConfigDumpInfo.xml` (опись выгрузки, где
/// перечислены ИМЕНА всех объектов, а не ссылки) НЕЛЬЗЯ — иначе каждый объект
/// «найдётся» сам на себя; этот файл отсеивает вызывающий (по имени).
///
/// `ЦветаСтиля.X`/`БиблиотекаКартинок.X` в XML не пишутся (там формы
/// `style:X`/`CommonPicture.X`), поэтому XML-разбор их не ищет.
pub fn extract_xml_usages(content: &str) -> Vec<CodeUsage> {
    let mut out: Vec<CodeUsage> = Vec::new();
    for (idx, raw) in content.split('\n').enumerate() {
        let lineno = idx + 1;
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        scan_xml_marker(
            raw,
            "CommonPicture.",
            "CommonPicture",
            "common_picture",
            lineno,
            &mut out,
        );
        scan_xml_marker(raw, "style:", "StyleItem", "style_item", lineno, &mut out);
    }
    out
}

/// Найти все вхождения `marker<Идент>` в одной строке XML и добавить обращения.
/// Идентификатор после маркера — буквы/цифры/`_` ([`is_word`]); пустой не даёт
/// обращения.
fn scan_xml_marker(
    line: &str,
    marker: &str,
    canonical: &str,
    kind: &'static str,
    lineno: usize,
    out: &mut Vec<CodeUsage>,
) {
    let mut from = 0usize;
    while let Some(pos) = line[from..].find(marker) {
        let start = from + pos + marker.len();
        let name: String = line[start..].chars().take_while(|c| is_word(*c)).collect();
        if !name.is_empty() {
            out.push(make_usage(canonical, &name, None, kind, lineno));
        }
        from = start;
        if from >= line.len() {
            break;
        }
    }
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

    /// То же для дополнительных каналов ([`extract_extra_usages`]): они живут
    /// отдельно от базового разбора, потому что пересекаются с ним по входу
    /// (макеты/картинки разобраны дважды — см. `extract_code_usages_for_index`).
    fn extra_kinds(c: &str) -> Vec<(String, Option<String>, &'static str)> {
        extract_extra_usages(c, None)
            .into_iter()
            .map(|u| (u.object_ref, u.member_path, u.usage_kind))
            .collect()
    }

    /// То же для текста запроса вне строковых литералов (`extract_query_usages`).
    fn qkinds(c: &str) -> Vec<(String, Option<String>, &'static str)> {
        extract_query_usages(c)
            .into_iter()
            .map(|u| (u.object_ref, u.member_path, u.usage_kind))
            .collect()
    }

    #[test]
    fn manager_in_code() {
        let r = kinds("Док = Документы.ПриобретениеТоваровУслуг.СоздатьДокумент();");
        assert_eq!(
            r,
            vec![(
                "Document.ПриобретениеТоваровУслуг".to_string(),
                None,
                "manager"
            )]
        );
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
    fn manager_after_metadata_prefix() {
        // `Метаданные.Справочники.X` — коллекция вторым сегментом (E-11).
        let r = kinds("Если ПравоДоступа(\"Чтение\", Метаданные.Справочники.Организации) Тогда");
        assert!(
            r.contains(&("Catalog.Организации".to_string(), None, "manager")),
            "обращение через объект Метаданные обязано попадать в индекс, получено: {r:?}"
        );
    }

    #[test]
    fn manager_document_journal_and_filter_criterion() {
        // Категории, которых раньше не было в таблице форм (E-10).
        let r = kinds("Ж = ЖурналыДокументов.ЖурналОпераций.СоставДокументов(\"X\");");
        assert_eq!(
            r,
            vec![(
                "DocumentJournal.ЖурналОпераций".to_string(),
                None,
                "manager"
            )]
        );
        let r = kinds("Т = КритерииОтбора.СвязанныеДокументы.Состав;");
        assert_eq!(
            r,
            vec![(
                "FilterCriterion.СвязанныеДокументы".to_string(),
                None,
                "manager"
            )]
        );
        let r = kinds("П = Последовательности.ПартииТоваров.СоздатьНаборЗаписей();");
        assert_eq!(
            r,
            vec![("Sequence.ПартииТоваров".to_string(), None, "manager")]
        );
    }

    #[test]
    fn ref_type_ru_in_string() {
        let r = kinds("Т = Тип(\"СправочникСсылка.Контрагенты\");");
        assert_eq!(
            r,
            vec![("Catalog.Контрагенты".to_string(), None, "ref_type")]
        );
    }

    #[test]
    fn ref_type_en_in_string() {
        let r = kinds("Т = Тип(\"DocumentRef.Заказ\");");
        assert_eq!(r, vec![("Document.Заказ".to_string(), None, "ref_type")]);
    }

    #[test]
    fn query_path_with_tabular() {
        // Путь метаданных в тексте запроса: 3-й сегмент = имя ТЧ.
        let r =
            kinds("Запрос.Текст = \"ВЫБРАТЬ Ссылка ИЗ Документ.РеализацияТоваровУслуг.Товары\";");
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
        assert_eq!(
            r,
            vec![(
                "InformationRegister.КурсыВалют".to_string(),
                None,
                "manager"
            )]
        );
    }

    #[test]
    fn query_usages_keep_virtual_tables_out_of_member_path() {
        // Регистр бухгалтерии: третий сегмент `Остатки` — виртуальная таблица
        // платформы, а не табличная часть, поэтому член объекта не пишется.
        let r = qkinds("ИЗ РегистрБухгалтерии.Хозрасчетный.Остатки КАК Остатки");
        assert_eq!(
            r,
            vec![("AccountingRegister.Хозрасчетный".to_string(), None, "query")]
        );
        // Английское написание виртуальной таблицы — тоже не член объекта.
        let r = extract_query_usages("ИЗ AccountingRegister.Хозрасчетный.Balance КАК Остатки");
        assert_eq!(r[0].object_ref, "AccountingRegister.Хозрасчетный");
        assert_eq!(r[0].member_path, None);

        // Справочник без третьего сегмента.
        let r = qkinds("ВЫБРАТЬ Ссылка ИЗ Справочник.Контрагенты");
        assert_eq!(r, vec![("Catalog.Контрагенты".to_string(), None, "query")]);

        // Документ с табличной частью: третий сегмент — член объекта.
        let r = qkinds("ВЫБРАТЬ Номенклатура ИЗ Документ.РеализацияТоваров.Товары");
        assert_eq!(
            r,
            vec![(
                "Document.РеализацияТоваров".to_string(),
                Some("Товары".to_string()),
                "query"
            )]
        );

        // Номер строки — физический, по переводам строк текста запроса.
        let r = extract_query_usages("ВЫБРАТЬ\nСсылка\nИЗ Справочник.Организации");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].line, 3);
        assert_eq!(r[0].usage_kind, "query");
    }

    // ── template_call / common_template_call (Требование 2 ТЗ, C2) ──────────

    #[test]
    fn template_call_from_literal() {
        let r = extract_extra_usages("Схема = ПолучитьМакет(\"ПФ_MXL_Вакансия\");", None);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].object_ref, "Template");
        assert_eq!(r[0].object_ref_key, "template");
        assert_eq!(r[0].member_path.as_deref(), Some("ПФ_MXL_Вакансия"));
        assert_eq!(r[0].usage_kind, "template_call");
        assert_eq!(r[0].line, 1);
    }

    #[test]
    fn common_template_call_from_literal() {
        let r = extract_extra_usages("Макет = ПолучитьОбщийМакет(\"ОбщаяПечатнаяФорма\");", None);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].object_ref, "CommonTemplate.ОбщаяПечатнаяФорма");
        assert_eq!(r[0].object_ref_key, "commontemplate.общаяпечатнаяформа");
        assert_eq!(r[0].member_path.as_deref(), Some("ОбщаяПечатнаяФорма"));
        assert_eq!(r[0].usage_kind, "common_template_call");
    }

    #[test]
    fn template_calls_accept_case_space_and_both_kinds_on_one_line() {
        let r = extract_extra_usages(
            "А = получитьмакет ( \"Первый\" ); Б = ПолучитьОбщийМакет(\"Второй\");",
            None,
        );
        let got: Vec<(&str, Option<&str>)> = r
            .iter()
            .map(|u| (u.usage_kind, u.member_path.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("template_call", Some("Первый")),
                ("common_template_call", Some("Второй")),
            ]
        );
    }

    #[test]
    fn template_calls_ignored_outside_real_code_or_without_single_literal() {
        // Комментарий.
        assert!(extra_kinds("// ПолучитьМакет(\"X\")").is_empty());
        // Внутри строкового литерала — это текст, а не вызов.
        assert!(extract_extra_usages("С = \"ПолучитьМакет(\"\"X\"\")\";", None).is_empty());
        // Аргумент — не один литерал: переменная, склейка, второй аргумент.
        assert!(extra_kinds("ПолучитьМакет(Имя)").is_empty());
        assert!(extra_kinds("ПолучитьМакет(\"X\" + Суффикс)").is_empty());
        assert!(extra_kinds("ПолучитьМакет(\"X\", Параметр)").is_empty());
        // Не наш вызов, лишь похожее имя.
        assert!(extra_kinds("МойПолучитьМакет(\"X\")").is_empty());
        // Пустой литерал и имя с пробелом — не идентификатор макета.
        assert!(extra_kinds("ПолучитьМакет(\"\")").is_empty());
        assert!(extra_kinds("ПолучитьМакет(\"Два слова\")").is_empty());
    }

    /// Метод-форма с менеджер-приёмником: владелец известен сразу (A1).
    #[test]
    fn template_call_via_manager_resolves_owner_from_receiver() {
        let r = extract_extra_usages("Схема = Справочники.Вакансии.ПолучитьМакет(\"ПФ\");", None);
        // Строка даёт и `manager` (Справочники.Вакансии), и сам вызов макета.
        let tpl: Vec<&CodeUsage> = r
            .iter()
            .filter(|u| u.usage_kind.starts_with("template"))
            .collect();
        assert_eq!(tpl.len(), 1, "{r:?}");
        assert_eq!(tpl[0].usage_kind, "template_call_via_manager");
        assert_eq!(tpl[0].object_ref, "Catalog.Вакансии");
        assert_eq!(tpl[0].object_ref_key, "catalog.вакансии");
        assert_eq!(tpl[0].member_path.as_deref(), Some("ПФ"));

        // EN-коллекция в одну папку выгрузки.
        let r = extract_extra_usages("Схема = Catalogs.Вакансии.ПолучитьМакет(\"ПФ\");", None);
        let tpl = r
            .iter()
            .find(|u| u.usage_kind.starts_with("template"))
            .expect("метод-форма извлечена");
        assert_eq!(tpl.object_ref, "Catalog.Вакансии");
        assert_eq!(tpl.usage_kind, "template_call_via_manager");
    }

    /// Метод-форма с нераспознанным приёмником — непокрытая форма (A1/A2).
    #[test]
    fn template_call_via_receiver_is_counted_not_resolved() {
        let r = extract_extra_usages("Схема = ОбъектПеременная.ПолучитьМакет(\"Y\");", None);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].usage_kind, "template_call_via_receiver");
        assert_eq!(r[0].object_ref, "Template");
        assert_eq!(r[0].object_ref_key, "template");
        assert_eq!(r[0].member_path.as_deref(), Some("Y"));
    }

    /// `Метаданные.Справочники.X` — не вызов макета и не менеджер-приёмник.
    #[test]
    fn metadata_prefix_does_not_confuse_manager_receiver() {
        // Обращение к метаданным без вызова макета: template-строк нет вовсе.
        let r = extract_extra_usages(
            "Если Метаданные.Справочники.Вакансии.Существует() Тогда",
            None,
        );
        assert!(
            r.iter().all(|u| !u.usage_kind.starts_with("template")),
            "Метаданные.Справочники.X не должен давать вызов макета: {r:?}"
        );
        // Вызов через `Метаданные.Справочники.X`: первый идентификатор цепочки
        // `Метаданные` не менеджер → владелец НЕ приписывается (непокрытая форма).
        let r = extract_extra_usages(
            "С = Метаданные.Справочники.Вакансии.ПолучитьМакет(\"Y\");",
            None,
        );
        let tpl: Vec<&CodeUsage> = r
            .iter()
            .filter(|u| u.usage_kind.starts_with("template"))
            .collect();
        assert_eq!(tpl.len(), 1, "{r:?}");
        assert_eq!(tpl[0].usage_kind, "template_call_via_receiver");
    }

    #[test]
    fn template_call_marker_does_not_break_neighbour_usages() {
        // Маркер литерала не должен ломать соседние извлечения на той же строке.
        // Здесь нужен весь набор индексатора: `manager` даёт базовый разбор,
        // `template_call` — дополнительные каналы.
        let r = extract_code_usages_for_index(
            "Док = Документы.Заказ.СоздатьДокумент(); Схема = ПолучитьМакет(\"Макет1\");",
            None,
        );
        let kinds_got: Vec<&str> = r.iter().map(|u| u.usage_kind).collect();
        assert!(kinds_got.contains(&"manager"), "{r:?}");
        assert!(kinds_got.contains(&"template_call"), "{r:?}");
    }

    #[test]
    fn template_call_line_number_is_physical() {
        let src = "А = 1;\n\nБ = ПолучитьМакет(\"Макет\");";
        let r = extract_extra_usages(src, None);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].line, 3);
    }

    /// Выравнивание маркеров и `strings` на строке с продолжением
    /// многострочного литерала (маркер продолжения идёт в начале строки).
    #[test]
    fn template_call_after_multiline_string_matches_its_own_literal() {
        let src = "Текст = \"начало\nпродолжение\"; Схема = ПолучитьМакет(\"Цель\");";
        let r = extract_extra_usages(src, None);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].usage_kind, "template_call");
        assert_eq!(r[0].member_path.as_deref(), Some("Цель"));
        assert_eq!(r[0].line, 2);
    }

    // ── резолв владельца макета при индексации ──────────────────────────────

    /// Владелец модуля известен: голая форма пишется РАЗРЕШЁННОЙ ссылкой —
    /// тогда её видят и рецепт `bsl_sql`, и `find_references`, и паспорт объекта.
    #[test]
    fn template_call_resolves_owner_from_module_path() {
        let r = extract_extra_usages("Схема = ПолучитьМакет(\"ПФ\");", Some("Catalog.Вакансии"));
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].object_ref, "Catalog.Вакансии.Template.ПФ");
        assert_eq!(r[0].object_ref_key, "catalog.вакансии.template.пф");
        assert_eq!(r[0].member_path.as_deref(), Some("ПФ"));
        assert_eq!(r[0].usage_kind, "template_call");
    }

    /// Приёмник-сам-объект (`ЭтотОбъект`, `Объект`): владелец макета — владелец модуля.
    #[test]
    fn template_call_resolves_self_receiver() {
        for src in [
            "Схема = ЭтотОбъект.ПолучитьМакет(\"ПФ\");",
            "Схема = Объект.ПолучитьМакет(\"ПФ\");",
        ] {
            let r = extract_extra_usages(src, Some("Document.Заказ"));
            let tpl = r
                .iter()
                .find(|u| u.usage_kind.starts_with("template"))
                .unwrap_or_else(|| panic!("вызов макета не найден: {src}"));
            assert_eq!(tpl.object_ref, "Document.Заказ.Template.ПФ", "{src}");
            assert_eq!(tpl.usage_kind, "template_call", "{src}");
        }
    }

    /// Без владельца модуля ссылка остаётся парой — резолв отдан потребителю.
    #[test]
    fn template_call_without_owner_stays_unresolved_pair() {
        let r = extract_extra_usages("Схема = ПолучитьМакет(\"ПФ\");", None);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].object_ref, "Template");
        assert_eq!(r[0].object_ref_key, "template");
        assert_eq!(r[0].member_path.as_deref(), Some("ПФ"));
        assert_eq!(r[0].usage_kind, "template_call");
    }

    /// Общий макет разрешается всегда: имя уникально в конфигурации, владелец не
    /// нужен — даже у метод-формы с нераспознанным приёмником.
    #[test]
    fn common_template_call_is_always_resolved() {
        let r = extract_extra_usages(
            "М = Переменная.ПолучитьОбщийМакет(\"ОбщаяПечатнаяФорма\");",
            None,
        );
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].object_ref, "CommonTemplate.ОбщаяПечатнаяФорма");
        assert_eq!(r[0].usage_kind, "common_template_call");
    }

    /// Путь модуля → владелец: раскладка выгрузки, модули форм и расширения.
    #[test]
    fn owner_ref_from_path_covers_dump_layouts() {
        assert_eq!(
            owner_ref_from_path("base/Catalogs/Вакансии/Ext/ManagerModule.bsl").as_deref(),
            Some("Catalog.Вакансии")
        );
        assert_eq!(
            owner_ref_from_path("base/Catalogs/Вакансии/Forms/ФормаЭлемента/Ext/Form/Module.bsl")
                .as_deref(),
            Some("Catalog.Вакансии")
        );
        assert_eq!(
            owner_ref_from_path("extensions/Расш/Documents/Заказ/Ext/ObjectModule.bsl").as_deref(),
            Some("Document.Заказ")
        );
        assert_eq!(
            owner_ref_from_path("base/Reports/Отчёт/Ext/ObjectModule.bsl").as_deref(),
            Some("Report.Отчёт")
        );
        // Общий модуль макетов не имеет — владельца нет.
        assert_eq!(
            owner_ref_from_path("CommonModules/ОбщегоНазначения/Ext/Module.bsl"),
            None
        );
    }

    /// Нелитеральный аргумент `ПолучитьМакет(Выражение)`: цель неизвестна, но
    /// владелец известен — строка-маркер, по которой его макеты попадают в корзину
    /// вычисляемых ссылок, а не в приговор.
    #[test]
    fn template_call_non_literal_marks_owner() {
        let r = extract_extra_usages(
            "Макет = ПолучитьМакет(ИмяМакета);",
            Some("Report.РегламентированныйОтчёт"),
        );
        let m = r
            .iter()
            .find(|u| u.usage_kind == "template_call_non_literal")
            .unwrap_or_else(|| panic!("маркер не найден: {r:?}"));
        assert_eq!(m.object_ref, "Report.РегламентированныйОтчёт");
        assert_eq!(m.object_ref_key, "report.регламентированныйотчёт");
        assert!(m.member_path.is_none(), "имя макета неизвестно: {m:?}");

        // Без владельца приписать некому — маркера нет.
        assert!(
            extract_extra_usages("Макет = ПолучитьМакет(ИмяМакета);", None)
                .iter()
                .all(|u| u.usage_kind != "template_call_non_literal")
        );

        // Общий макет: корзина вычисляемых ссылок — про макеты владельца, тут его нет.
        assert!(
            extract_extra_usages("Макет = ПолучитьОбщийМакет(ИмяМакета);", Some("Report.X"))
                .is_empty()
        );

        // Менеджер-приёмник даёт владельца даже без пути модуля.
        let r = extract_extra_usages("Макет = Справочники.ВидыНалогов.ПолучитьМакет(Имя);", None);
        let m = r
            .iter()
            .find(|u| u.usage_kind == "template_call_non_literal")
            .unwrap_or_else(|| panic!("маркер не найден: {r:?}"));
        assert_eq!(m.object_ref, "Catalog.ВидыНалогов");
    }

    /// Склейка, второй аргумент и литерал не-идентификатор — та же неясная цель.
    #[test]
    fn template_call_non_literal_covers_concat_and_bad_literal() {
        for src in [
            "Макет = ПолучитьМакет(\"Форма\" + Номер);",
            "Макет = ПолучитьМакет(\"Форма\", Номер);",
            "Макет = ПолучитьМакет(\"Форма.СПодстановкой\");",
        ] {
            let r = extract_extra_usages(src, Some("Report.X"));
            assert!(
                r.iter()
                    .any(|u| u.usage_kind == "template_call_non_literal"),
                "{src}: {r:?}"
            );
        }
    }

    /// Приёмник-выражение (`ПолучитьМакетОбъекта(Ссылка).ПолучитьМакет("X")`):
    /// владельца из приёмника не вывести, но при известном пути модуля макет
    /// приписывается владельцу модуля — как делал канал 1.10.0. Вид сохраняется,
    /// чтобы число таких мест оставалось видно счётчиком.
    #[test]
    fn template_call_via_receiver_resolves_owner_when_known() {
        let src = "Схема = ОбщегоНазначения.ПолучитьМакетОбъекта(Ссылка).ПолучитьМакет(\"ПФ\");";
        let r = extract_extra_usages(src, Some("Report.РегламентированныйОтчёт"));
        let tpl = r
            .iter()
            .find(|u| u.usage_kind.starts_with("template"))
            .unwrap_or_else(|| panic!("вызов макета не найден: {r:?}"));
        assert_eq!(tpl.object_ref, "Report.РегламентированныйОтчёт.Template.ПФ");
        assert_eq!(tpl.usage_kind, "template_call_via_receiver");

        // Без владельца модуля та же строка остаётся парой-счётчиком.
        let r = extract_extra_usages(src, None);
        let tpl = r
            .iter()
            .find(|u| u.usage_kind.starts_with("template"))
            .unwrap_or_else(|| panic!("вызов макета не найден: {r:?}"));
        assert_eq!(tpl.object_ref, "Template");
        assert_eq!(tpl.usage_kind, "template_call_via_receiver");
    }

    // ── common_module_via_literal (F1b: строковый доступ к общему модулю) ─────

    #[test]
    fn common_module_literal_in_code_is_usage() {
        let r = extra_kinds("МодульСобытия = ОбщийМодуль(\"ОбменДаннымиСобытия\");");
        assert_eq!(
            r,
            vec![(
                "CommonModule.ОбменДаннымиСобытия".to_string(),
                None,
                "common_module_via_literal"
            )]
        );
    }

    #[test]
    fn common_module_literal_en_form_and_inner_spaces() {
        let r = extra_kinds("M = CommonModule( \"CommonModuleName\" );");
        assert_eq!(
            r,
            vec![(
                "CommonModule.CommonModuleName".to_string(),
                None,
                "common_module_via_literal"
            )]
        );
    }

    /// Непокрытые формы не извлекаются вовсе: имя не литерал, склейка, второй
    /// аргумент, метод чужого объекта, имя с пробелом, часть длинного
    /// идентификатора (`ОбщегоНазначенияОбщийМодуль(…)` — обёртка БСП).
    #[test]
    fn common_module_literal_ignored_outside_real_code_or_without_single_literal() {
        assert!(extra_kinds("Модуль = ОбщийМодуль(Имя);").is_empty());
        assert!(extra_kinds("Модуль = ОбщийМодуль(\"Обмен\" + Суффикс);").is_empty());
        assert!(extra_kinds("Модуль = ОбщийМодуль(\"Обмен\", Параметр);").is_empty());
        assert!(extra_kinds("Модуль = ОбщийМодуль(\"Обмен Данными\");").is_empty());
        assert!(extra_kinds("Модуль = ОбщийМодуль(\"\");").is_empty());
        assert!(extra_kinds("Модуль = Переменная.ОбщийМодуль(\"Обмен\");").is_empty());
        assert!(extra_kinds("Модуль = ОбщегоНазначенияОбщийМодуль(\"Обмен\");").is_empty());
        assert!(extra_kinds("// ОбщийМодуль(\"Обмен\")").is_empty());
        assert!(extra_kinds("С = \"ОбщийМодуль(\"\"Обмен\"\")\";").is_empty());
    }

    /// Соседние извлечения на той же строке не ломаются (маркер литерала).
    #[test]
    fn common_module_literal_does_not_break_neighbour_usages() {
        // Весь набор индексатора: `manager` приходит из базового разбора,
        // наш строковый канал — из дополнительных.
        let r = extract_code_usages_for_index(
            "Схема = ПолучитьМакет(\"Макет1\"); М = ОбщийМодуль(\"Обмен\"); Д = Документы.Заказ;",
            None,
        );
        let got: Vec<(&str, &str)> = r
            .iter()
            .map(|u| (u.object_ref.as_str(), u.usage_kind))
            .collect();
        assert!(
            got.contains(&("CommonModule.Обмен", "common_module_via_literal")),
            "{r:?}"
        );
        assert!(
            r.iter()
                .any(|u| u.usage_kind == "template_call"
                    && u.member_path.as_deref() == Some("Макет1")),
            "{r:?}"
        );
        assert!(got.contains(&("Document.Заказ", "manager")), "{r:?}");
    }

    // ── common_picture / style_item (D1: ссылки на картинки и элементы стиля) ──

    #[test]
    fn picture_library_in_code_is_common_picture() {
        // RU-форма обращения к общей библиотеке картинок (реальный синтаксис).
        let r = extra_kinds("Картинка = БиблиотекаКартинок.Комментарий;");
        assert_eq!(
            r,
            vec![(
                "CommonPicture.Комментарий".to_string(),
                None,
                "common_picture"
            )]
        );
        // EN-форма свойства глобального контекста.
        let r = extra_kinds("Pic = PictureLibrary.Answer;");
        assert_eq!(
            r,
            vec![("CommonPicture.Answer".to_string(), None, "common_picture")]
        );
        // Форма метаданного (в XML встречается как CommonPicture.X).
        let r = extra_kinds("P = CommonPicture.Информация32;");
        assert_eq!(
            r,
            vec![(
                "CommonPicture.Информация32".to_string(),
                None,
                "common_picture"
            )]
        );
    }

    #[test]
    fn style_collection_in_code_is_style_item() {
        let r = extra_kinds("Цвет = ЦветаСтиля.ПросроченныеДанныеЦвет;");
        assert_eq!(
            r,
            vec![(
                "StyleItem.ПросроченныеДанныеЦвет".to_string(),
                None,
                "style_item"
            )]
        );
        let r = extra_kinds("Ш = ШрифтыСтиля.КрупныйШрифтТекста;");
        assert_eq!(
            r,
            vec![(
                "StyleItem.КрупныйШрифтТекста".to_string(),
                None,
                "style_item"
            )]
        );
        let r = extra_kinds("C = StyleColors.AccentColor;");
        assert_eq!(
            r,
            vec![("StyleItem.AccentColor".to_string(), None, "style_item")]
        );
    }

    #[test]
    fn picture_and_style_ignored_in_strings_and_comments() {
        // В строке/комментарии это текст, а не обращение.
        assert!(extra_kinds("С = \"БиблиотекаКартинок.Папка\";").is_empty());
        assert!(extra_kinds("// ЦветаСтиля.ЦветФонаФормы").is_empty());
    }

    /// E4: метод коллекции и доступ к полю НЕ являются ссылками на объекты
    /// метаданных (ложный класс «метод библиотеки / доступ к полю»).
    #[test]
    fn picture_library_method_and_field_access_are_not_usages() {
        // (а) метод коллекции: `БиблиотекаКартинок.Есть(Имя)` — метод, а не
        // обращение к объекту `CommonPicture.Есть`.
        assert!(
            extra_kinds("Если БиблиотекаКартинок.Есть(Имя) Тогда").is_empty(),
            "метод библиотеки картинок не должен давать common_picture"
        );
        // (б) доступ к полю объекта: `<Объект>.БиблиотекаКартинок.Комментарий`.
        assert!(
            extra_kinds("К = МойОбъект.БиблиотекаКартинок.Комментарий;").is_empty(),
            "поле объекта не должно давать common_picture"
        );
        // То же правило для элементов стиля.
        assert!(extra_kinds("Ц = МойОбъект.ЦветаСтиля.ЦветФона;").is_empty());
        assert!(extra_kinds("Ш = МойОбъект.ШрифтыСтиля.Шрифт;").is_empty());
        assert!(extra_kinds("Р = МойОбъект.РамкиСтиля.Рамка;").is_empty());
        // Настоящее обращение (без метода/поля) остаётся видимым.
        let r = extra_kinds("К = БиблиотекаКартинок.Комментарий;");
        assert_eq!(
            r,
            vec![(
                "CommonPicture.Комментарий".to_string(),
                None,
                "common_picture"
            )]
        );
        // И обычная ссылка на элемент стиля тоже.
        let r = extra_kinds("Ц = ЦветаСтиля.ПросроченныеДанныеЦвет;");
        assert_eq!(
            r,
            vec![(
                "StyleItem.ПросроченныеДанныеЦвет".to_string(),
                None,
                "style_item"
            )]
        );
    }

    #[test]
    fn xml_usages_extract_picture_and_style_forms() {
        let xml = r#"<Form>
  <Picture><xr:Ref>CommonPicture.Напоминание</xr:Ref></Picture>
  <TextColor>style:ПросроченныеДанныеЦвет</TextColor>
  <dcscor:value xsi:type="v8ui:Color">style:НедоступныеДанныеЦвет</dcscor:value>
</Form>"#;
        let r = extract_xml_usages(xml);
        let got: Vec<(String, &str, usize)> = r
            .iter()
            .map(|u| (u.object_ref.clone(), u.usage_kind, u.line))
            .collect();
        assert_eq!(
            got,
            vec![
                ("CommonPicture.Напоминание".to_string(), "common_picture", 2),
                (
                    "StyleItem.ПросроченныеДанныеЦвет".to_string(),
                    "style_item",
                    3
                ),
                (
                    "StyleItem.НедоступныеДанныеЦвет".to_string(),
                    "style_item",
                    4
                ),
            ],
            "{r:?}"
        );
        // Ключи в нижнем регистре — регистронезависимый поиск без UDF.
        assert_eq!(r[1].object_ref_key, "styleitem.просроченныеданныецвет");
    }

    #[test]
    fn xml_usages_ignore_namespace_and_manifest_like_text() {
        // `xmlns:style="..."` — объявление пространства имён, не ссылка.
        let xml = r#"<MetaDataObject xmlns:style="http://v8.1c.ru/8.1/data/ui/style" xmlns:xsi="x" version="2.13"/>"#;
        assert!(
            extract_xml_usages(xml).is_empty(),
            "{:?}",
            extract_xml_usages(xml)
        );
        // Пустой идентификатор после маркера не даёт обращения.
        assert!(extract_xml_usages("<a>style:</a>").is_empty());
    }
}
