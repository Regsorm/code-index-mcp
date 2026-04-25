// Парсинг `ConfigDumpInfo.xml` — служебного файла рядом с
// `Configuration.xml`, который содержит хеши версий каждого
// объекта/формы/модуля конфигурации.
//
// Файл создаётся платформой 1С при `DumpConfigToFiles`, лежит в том же
// каталоге что и `Configuration.xml`. Структура (упрощённо):
//
// ```xml
// <ConfigDumpInfo configVersion="...">
//   <Metadata name="Catalog.Контрагенты" id="aaaaaaaa-..." configVersion="42a1b2..."/>
//   <Metadata name="Catalog.Контрагенты.Form.ФормаЭлемента" id="bbbbbbbb-..." configVersion="ce93..."/>
//   <Metadata name="Catalog.Контрагенты.Form.ФормаЭлемента.Module"
//             id="bbbbbbbb-...Module" configVersion="..."/>
//   ...
// </ConfigDumpInfo>
// ```
//
// Что нам интересует:
// * `id` без точки — это «чистый» UUID объекта или формы.
//   Берём в map (uuid → configVersion).
// * `id` с точкой — это suffix к UUID «.Module» / «.ManagerModule» / ...,
//   таких записей миллионы для большой конфигурации, и configVersion
//   модуля совпадает с configVersion владельца. Не храним отдельно.
//
// Файл может быть тяжёлым (десятки МБ). Используем `quick_xml` event-стрим
// без построения DOM.

use anyhow::Result;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;
use std::path::Path;

/// Прочитать `ConfigDumpInfo.xml` и вернуть карту UUID → configVersion
/// для всех объектов и форм. Записи с suffix-id (`uuid.Module`) пропускаются.
///
/// Возвращает пустой map если файл отсутствует или нечитаемый — ошибки
/// не бросаются, потому что `ConfigDumpInfo.xml` опционален: для свежей
/// выгрузки он есть, для архивных — может быть удалён.
pub fn parse_config_dump_info(cfg_root: &Path) -> Result<HashMap<String, String>> {
    let path = cfg_root.join("ConfigDumpInfo.xml");
    if !path.is_file() {
        return Ok(HashMap::new());
    }
    let content = std::fs::read_to_string(&path)?;
    Ok(parse_config_dump_info_str(&content))
}

/// Тот же парсинг, но из строки (для тестов и mock-сценариев).
pub fn parse_config_dump_info_str(xml: &str) -> HashMap<String, String> {
    let mut result: HashMap<String, String> = HashMap::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                // Тег может быть с namespace — ловим по local-name.
                let name = e.name();
                let raw = name.as_ref();
                let tag = std::str::from_utf8(raw).unwrap_or("");
                let local = tag.split(':').last().unwrap_or(tag);
                if local != "Metadata" {
                    continue;
                }
                let mut id: Option<String> = None;
                let mut config_version: Option<String> = None;
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"id" => {
                            id = attr
                                .unescape_value()
                                .ok()
                                .map(|cow| cow.to_string());
                        }
                        b"configVersion" => {
                            config_version = attr
                                .unescape_value()
                                .ok()
                                .map(|cow| cow.to_string());
                        }
                        _ => {}
                    }
                }
                if let (Some(id), Some(cv)) = (id, config_version) {
                    // Пропускаем suffix-id (`uuid.Module` / `uuid.FormModule` / ...) —
                    // configVersion модуля = configVersion владельца, дубликат
                    // не нужен.
                    if !id.contains('.') {
                        result.insert(id, cv);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_uuids_only() {
        let xml = r#"<?xml version="1.0"?>
<ConfigDumpInfo configVersion="rootver">
  <Metadata name="Catalog.X" id="aaaaaaaa-1111-2222-3333-444444444444" configVersion="catver"/>
  <Metadata name="Catalog.X.Form.F" id="bbbbbbbb-5555-6666-7777-888888888888" configVersion="formver"/>
  <Metadata name="Catalog.X.Form.F.Module" id="bbbbbbbb-5555-6666-7777-888888888888.Module" configVersion="formver"/>
  <Metadata name="Catalog.X.ObjectModule" id="aaaaaaaa-1111-2222-3333-444444444444.ObjectModule" configVersion="catver"/>
</ConfigDumpInfo>"#;
        let map = parse_config_dump_info_str(xml);
        // Только два чистых UUID — у каталога и у формы.
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get("aaaaaaaa-1111-2222-3333-444444444444").map(String::as_str),
            Some("catver")
        );
        assert_eq!(
            map.get("bbbbbbbb-5555-6666-7777-888888888888").map(String::as_str),
            Some("formver")
        );
    }

    #[test]
    fn empty_xml_returns_empty_map() {
        assert!(parse_config_dump_info_str("").is_empty());
    }

    #[test]
    fn no_metadata_elements_returns_empty() {
        let xml = "<ConfigDumpInfo><Other/></ConfigDumpInfo>";
        assert!(parse_config_dump_info_str(xml).is_empty());
    }

    #[test]
    fn missing_attributes_skipped() {
        let xml = r#"<ConfigDumpInfo>
            <Metadata name="X"/>
            <Metadata name="Y" id="some-id-without-cv"/>
            <Metadata name="Z" configVersion="cv-without-id"/>
            <Metadata name="OK" id="cccccccc-1234-5678-90ab-cdef00112233" configVersion="okver"/>
        </ConfigDumpInfo>"#;
        let map = parse_config_dump_info_str(xml);
        assert_eq!(map.len(), 1);
        assert_eq!(
            map.get("cccccccc-1234-5678-90ab-cdef00112233").map(String::as_str),
            Some("okver")
        );
    }

    #[test]
    fn does_not_panic_on_malformed_xml() {
        let _ = parse_config_dump_info_str("<<<not really xml>");
        let _ = parse_config_dump_info_str("</wrong>");
    }
}
