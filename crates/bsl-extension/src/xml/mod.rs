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

pub mod config_dump_info;
pub mod configuration;
pub mod event_subscriptions;
pub mod forms;
pub mod object_uuid;
