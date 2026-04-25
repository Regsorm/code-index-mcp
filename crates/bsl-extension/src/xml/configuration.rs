// Парсер `Configuration.xml` — корневого файла выгрузки конфигурации 1С.
//
// Структура файла (упрощённо):
//
// <MetaDataObject xmlns:v8="...">
//   <Configuration>
//     <Properties>
//       <Name>Конфигурация</Name>
//       <Synonym><v8:item lang="ru"><v8:content>Конфигурация ...</v8:content>...
//     </Properties>
//     <ChildObjects>
//       <Subsystem>ПодсистемаА</Subsystem>
//       <Catalog>Контрагенты</Catalog>
//       <Catalog>Номенклатура</Catalog>
//       <Document>РеализацияТоваровУслуг</Document>
//       ...
//     </ChildObjects>
//   </Configuration>
// </MetaDataObject>
//
// Парсер возвращает список `ObjectRef { meta_type, name, full_name }`
// для всех объектов в `<ChildObjects>`. Это «оглавление» конфигурации —
// для каждого объекта потом будет отдельный XML-файл (у Catalog,
// Document, ChartOfCharacteristicTypes и т.д.) со структурой реквизитов.
// Чтение этих файлов делается отдельно и записывает строку в
// `metadata_objects`. Configuration.xml лишь сообщает «какие объекты есть».

use std::path::Path;

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

/// Один объект конфигурации, перечисленный в Configuration.xml/<ChildObjects>.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRef {
    /// Тип объекта: "Catalog", "Document", "InformationRegister", ...
    pub meta_type: String,
    /// Имя объекта без префикса: "Контрагенты", "РеализацияТоваровУслуг".
    pub name: String,
    /// Канонический идентификатор: `<meta_type>.<name>`.
    pub full_name: String,
}

/// Имена тегов, которые мы понимаем как тип объекта внутри `<ChildObjects>`.
/// Совпадает с `METADATA_TYPES` в core::parser::xml_1c, но мы держим список
/// здесь, чтобы bsl-extension не зависела от деталей core-парсера.
const KNOWN_META_TYPES: &[&str] = &[
    "Subsystem",
    "Catalog",
    "Document",
    "Enum",
    "Constant",
    "InformationRegister",
    "AccumulationRegister",
    "AccountingRegister",
    "CalculationRegister",
    "DataProcessor",
    "Report",
    "CommonModule",
    "ChartOfCharacteristicTypes",
    "ChartOfAccounts",
    "ChartOfCalculationTypes",
    "ExchangePlan",
    "BusinessProcess",
    "Task",
    "DocumentJournal",
    "FilterCriterion",
    "EventSubscription",
    "ScheduledJob",
    "FunctionalOption",
    "FunctionalOptionsParameter",
    "DefinedType",
    "CommonAttribute",
    "SettingsStorage",
    "WSReference",
    "WebService",
    "HTTPService",
    "Style",
    "Language",
    "SessionParameter",
    "CommonForm",
    "CommonCommand",
    "CommandGroup",
    "CommonTemplate",
    "CommonPicture",
    "XDTOPackage",
    "Sequence",
    "Bot",
    "ExternalDataSource",
];

/// Распарсить содержимое Configuration.xml в список объектов.
pub fn parse_configuration_xml(content: &str) -> Result<Vec<ObjectRef>> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    let mut buf = Vec::new();
    let mut tag_stack: Vec<String> = Vec::new();
    let mut in_child_objects = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = local_name(e.name().as_ref());
                if local == "ChildObjects" {
                    in_child_objects = true;
                }
                tag_stack.push(local);
            }
            Ok(Event::End(e)) => {
                let local = local_name(e.name().as_ref());
                if local == "ChildObjects" {
                    in_child_objects = false;
                }
                tag_stack.pop();
            }
            Ok(Event::Text(text)) => {
                if !in_child_objects {
                    continue;
                }
                let parent = tag_stack.last().map(|s| s.as_str()).unwrap_or("");
                if KNOWN_META_TYPES.contains(&parent) {
                    let name = text
                        .unescape()
                        .map(|s| s.into_owned())
                        .unwrap_or_default();
                    let name = name.trim().to_string();
                    if !name.is_empty() {
                        let full_name = format!("{}.{}", parent, name);
                        out.push(ObjectRef {
                            meta_type: parent.to_string(),
                            name,
                            full_name,
                        });
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Configuration.xml: ошибка парсинга на позиции {}: {}",
                    reader.buffer_position(),
                    e
                ));
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Прочитать и распарсить файл Configuration.xml по пути.
/// Возвращает `Ok(Vec::new())`, если файла нет — это валидно для репо,
/// который ещё не выгружен через DumpConfigToFiles.
pub fn parse_configuration_file(path: &Path) -> Result<Vec<ObjectRef>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Не удалось прочитать {}", path.display()))?;
    parse_configuration_xml(&content)
}

/// Извлечь имя тега без namespace-префикса (`v8:item` → `item`).
fn local_name(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name).into_owned();
    match s.find(':') {
        Some(idx) => s[idx + 1..].to_string(),
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Configuration uuid="abc">
    <Properties>
      <Name>Конфигурация</Name>
    </Properties>
    <ChildObjects>
      <Subsystem>Продажи</Subsystem>
      <Catalog>Контрагенты</Catalog>
      <Catalog>Номенклатура</Catalog>
      <Document>РеализацияТоваровУслуг</Document>
      <CommonModule>ОбщегоНазначенияСервер</CommonModule>
      <UnknownTagShouldBeSkipped>foo</UnknownTagShouldBeSkipped>
    </ChildObjects>
  </Configuration>
</MetaDataObject>
"#;

    #[test]
    fn parses_known_meta_types_only() {
        let objs = parse_configuration_xml(SAMPLE).unwrap();
        let names: Vec<String> = objs.iter().map(|o| o.full_name.clone()).collect();
        assert_eq!(
            names,
            vec![
                "Subsystem.Продажи",
                "Catalog.Контрагенты",
                "Catalog.Номенклатура",
                "Document.РеализацияТоваровУслуг",
                "CommonModule.ОбщегоНазначенияСервер",
            ],
            "должны попасть только теги из KNOWN_META_TYPES",
        );
    }

    #[test]
    fn skips_text_outside_child_objects() {
        // <Name> в <Properties> — это имя самой конфигурации, не объект,
        // его в ChildObjects нет, поэтому он не должен попасть в результат.
        let objs = parse_configuration_xml(SAMPLE).unwrap();
        assert!(objs.iter().all(|o| o.name != "Конфигурация"));
    }

    #[test]
    fn returns_empty_for_missing_file() {
        let nonexistent = std::path::Path::new("/never/exists/here/Configuration.xml");
        let objs = parse_configuration_file(nonexistent).unwrap();
        assert!(objs.is_empty());
    }

    #[test]
    fn splits_meta_type_and_name_correctly() {
        let objs = parse_configuration_xml(SAMPLE).unwrap();
        let catalog = objs
            .iter()
            .find(|o| o.full_name == "Catalog.Контрагенты")
            .unwrap();
        assert_eq!(catalog.meta_type, "Catalog");
        assert_eq!(catalog.name, "Контрагенты");
    }
}
