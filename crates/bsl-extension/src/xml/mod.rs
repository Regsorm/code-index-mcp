// Парсеры XML-выгрузок 1С, специфичные для bsl-extension.
//
// Эти парсеры дополняют generic `Xml1CParser` из core (который видит XML
// как набор «классов»). Здесь — структурированное извлечение метаданных,
// предназначенное для записи в специфичные таблицы:
//
// - `configuration` — Configuration.xml: список всех объектов конфигурации
//   (Catalog/Document/InformationRegister/...) с их именами, синонимами
//   и UUID. Источник для таблицы `metadata_objects`.
// - `forms` — *.xml в Forms/: имена обработчиков событий формы. Источник
//   для `metadata_forms`.
// - `event_subscriptions` — *.xml в EventSubscriptions/: связь
//   «событие → модуль.процедура». Источник для `event_subscriptions`.
// - `object_attributes` — XML отдельных объектов (Catalogs/<X>.xml и т.д.):
//   ссылочные типы реквизитов/измерений → рёбра графа связей данных.
//   Источник для `data_links`.
// - `metadata_refs` — связи КОНФИГУРАЦИОННОГО уровня (состав подсистем и
//   планов обмена, типы определяемых типов, расположение функциональных
//   опций) → доп. рёбра `data_links`; плюс права ролей (Rights.xml) для
//   отдельной таблицы `role_rights`.
// - `dcs` — макеты «Схема компоновки данных» (`Templates/<Имя>/Ext/Template.xml`
//   и `Template.dcs` выгрузки EDT): наборы данных с полями, связи наборов,
//   вычисляемые поля, итоги, параметры и варианты настроек. Источник для
//   таблиц `dcs_schemas` / `dcs_datasets` и рёбер `data_links` вида `dcs_query`.

use std::borrow::Cow;

use quick_xml::events::{BytesRef, BytesText};

/// Совместимый с прежним quick-xml путь: декодировать XML-текст и раскрыть
/// стандартные entity (`&amp;`, `&lt;` и т.д.).
pub(crate) trait BytesTextExt {
    fn unescape(&self) -> Result<Cow<'_, str>, ()>;
}

impl BytesTextExt for BytesText<'_> {
    fn unescape(&self) -> Result<Cow<'_, str>, ()> {
        let decoded = self.xml10_content().map_err(|_| ())?;
        let unescaped = quick_xml::escape::unescape(&decoded).map_err(|_| ())?;
        Ok(Cow::Owned(unescaped.into_owned()))
    }
}

/// Текст ссылки на сущность — события `Event::GeneralRef`.
///
/// С quick-xml 0.38 сущности внутри текста (`&amp;`, `&lt;`, `&#38;`) больше
/// не входят в `Event::Text`, а приходят отдельным событием между двумя
/// текстовыми. Разборщик, который его не ловит, молча теряет символ: выражение
/// СКД `Код в (&amp;Параметр)` превращалось в `Код в (Параметр)`. Стандартные
/// имена и числовые ссылки раскрываются; незнакомая сущность возвращается как
/// была — `&имя;` — чтобы текст хотя бы не искажался молча.
pub(crate) fn general_ref_text(r: &BytesRef<'_>) -> String {
    if let Ok(Some(ch)) = r.resolve_char_ref() {
        return ch.to_string();
    }
    let name = r.decode().map(|c| c.into_owned()).unwrap_or_default();
    match name.as_str() {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" => "'".to_string(),
        other => format!("&{other};"),
    }
}

pub mod config_dump_info;
pub mod configuration;
pub mod dcs;
// `edt_mdo` — формат 1C:EDT (`.mdo`): структура объектов, связи данных,
// синоним/шапка. Заполняет те же таблицы, что и формат Конфигуратора.
pub mod edt_mdo;
pub mod event_subscriptions;
pub mod forms;
pub mod metadata_refs;
pub mod object_attributes;
pub mod object_uuid;
