// Хардкод-константы платформы 1С для отладочных идентификаторов.
//
// Источник истины — DLL платформы (Designer 8.3.x). Те же константы
// использует наш сервис dbgs-debug для установки точек останова через
// HTTP-протокол отладки 1С (`property_id` в JSON-payload `setBreakpoint`).
//
// Дублируем их сюда вместо «share between Python и Rust», потому что
// этот словарь стабилен между версиями платформы — менялся последний раз
// очень давно. Если когда-нибудь Anthropic 1С добавит новый тип модуля
// — обновим здесь и в dbgs-debug одновременно.

/// Имя BSL-файла → канонический тип модуля 1С.
///
/// Используется при обходе дерева конфигурации: имя последнего сегмента
/// пути `.bsl`-файла однозначно классифицирует модуль. Module.bsl
/// — особый случай: и `CommonModule`, и `HTTPService`/`WebService`
/// называют свой главный модуль `Module.bsl`. Финальный
/// `module_type = "Module"` для них всех корректен (платформа использует
/// один и тот же `property_id` под все три).
pub const BSL_FILE_MODULE_TYPE: &[(&str, &str)] = &[
    ("ObjectModule.bsl", "ObjectModule"),
    ("ManagerModule.bsl", "ManagerModule"),
    ("RecordSetModule.bsl", "RecordSetModule"),
    ("ValueManagerModule.bsl", "ValueManagerModule"),
    ("CommandModule.bsl", "CommandModule"),
    // Module.bsl лежит у CommonModule, HTTPService, WEBService —
    // тип одинаковый «Module» (см. property_id ниже).
    ("Module.bsl", "Module"),
    ("ManagedApplicationModule.bsl", "ManagedApplicationModule"),
    ("SessionModule.bsl", "SessionModule"),
    ("ExternalConnectionModule.bsl", "ExternalConnectionModule"),
    ("OrdinaryApplicationModule.bsl", "OrdinaryApplicationModule"),
];

/// Тип модуля → `property_id` (UUID платформы) для протокола отладки 1С.
///
/// Эти UUID — внутренние GUID классов платформы, передаются dbgs в
/// `setBreakpoint` для указания «в каком именно модуле остановиться».
/// Без них точка останова не привязывается к строке.
pub const MODULE_TYPE_PROPERTY_ID: &[(&str, &str)] = &[
    ("ObjectModule",                "a637f77f-3840-441d-a1c3-699c8c5cb7e0"),
    ("ManagerModule",               "d1b64a2c-8078-4982-8190-8f81aefda192"),
    ("FormModule",                  "32e087ab-1491-49b6-aba7-43571b41ac2b"),
    ("RecordSetModule",             "9f36fd70-4bf4-47f6-b235-935f73aab43f"),
    ("CommandModule",               "078a6af8-d22c-4248-9c33-7e90075a3d2c"),
    ("ValueManagerModule",          "3e58c91f-9aaa-4f42-8999-4baf33907b75"),
    // Module = CommonModule / HTTPService / WebService
    ("Module",                      "d5963243-262e-4398-b4d7-fb16d06484f6"),
    ("ManagedApplicationModule",    "d22e852a-cf8a-4f77-8ccb-3548e7792bea"),
    ("SessionModule",               "9b7bbbae-9771-46f2-9e4d-2489e0ffc702"),
    ("ExternalConnectionModule",    "a4a9c1e2-1e54-4c7f-af06-4ca341198fac"),
    ("OrdinaryApplicationModule",   "a78d9ce3-4e0c-48d5-9863-ae7342eedf94"),
];

/// Тип модуля по имени `.bsl`-файла. None если имя не из известного
/// набора — эти .bsl-файлы пропускаются индексером модулей (но
/// core всё равно их парсит как обычные функции/процедуры).
pub fn module_type_by_filename(file_name: &str) -> Option<&'static str> {
    BSL_FILE_MODULE_TYPE
        .iter()
        .find(|(name, _)| *name == file_name)
        .map(|(_, t)| *t)
}

/// `property_id` по типу модуля. None если тип неизвестен.
pub fn property_id_by_type(module_type: &str) -> Option<&'static str> {
    MODULE_TYPE_PROPERTY_ID
        .iter()
        .find(|(t, _)| *t == module_type)
        .map(|(_, id)| *id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_type_lookup_works() {
        assert_eq!(module_type_by_filename("ManagerModule.bsl"), Some("ManagerModule"));
        assert_eq!(module_type_by_filename("Module.bsl"), Some("Module"));
        assert_eq!(module_type_by_filename("ObjectModule.bsl"), Some("ObjectModule"));
        assert_eq!(module_type_by_filename("Random.bsl"), None);
    }

    #[test]
    fn property_id_lookup_works() {
        // Совпадает со словарём dbgs-debug
        assert_eq!(
            property_id_by_type("Module"),
            Some("d5963243-262e-4398-b4d7-fb16d06484f6")
        );
        assert_eq!(
            property_id_by_type("ManagerModule"),
            Some("d1b64a2c-8078-4982-8190-8f81aefda192")
        );
        assert_eq!(property_id_by_type("UnknownType"), None);
    }

    #[test]
    fn all_module_types_have_property_ids() {
        // Каждый тип в BSL_FILE_MODULE_TYPE должен иметь property_id.
        // Защита от опечаток / расхождений между двумя списками.
        for (_file, mtype) in BSL_FILE_MODULE_TYPE {
            assert!(
                property_id_by_type(mtype).is_some(),
                "Тип модуля '{}' не имеет property_id — нарушена синхронизация словарей",
                mtype
            );
        }
    }

    #[test]
    fn form_module_has_property_id_even_without_file() {
        // FormModule НЕ присутствует в BSL_FILE_MODULE_TYPE (модуль формы
        // лежит в Forms/<Name>/Ext/Form/Module.bsl и определяется иначе),
        // но property_id для него обязан существовать.
        assert!(property_id_by_type("FormModule").is_some());
    }
}
