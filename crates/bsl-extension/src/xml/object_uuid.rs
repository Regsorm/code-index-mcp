// Извлечение UUID объекта 1С из его XML-выгрузки.
//
// Структура XML типичного объекта (`Documents/X.xml`, `Catalogs/X.xml`,
// `CommonModules/X.xml` и т.д.):
//
// ```xml
// <?xml version="1.0" encoding="UTF-8"?>
// <MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" ...>
//   <Document uuid="12f1d8bf-4a3c-4d51-9e0a-...">
//     <Properties>...</Properties>
//   </Document>
// </MetaDataObject>
// ```
//
// Тег внутри `MetaDataObject` (Document/Catalog/CommonModule/...)
// нас не интересует — берём атрибут `uuid` у первого дочернего элемента.
//
// Для форм (`Forms/<FormName>/Form.xml`) структура немного другая:
// ```xml
// <Form xmlns="http://v8.1c.ru/8.3/xcf/logform" ...
//       uuid="b4d8a4b7-..." ...>
// ```
// Здесь `uuid` — атрибут самого корня. Используется отдельная функция
// `extract_form_uuid_from_xml_str`.

use anyhow::Result;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::path::Path;

/// Извлечь UUID из XML объекта (Documents/X.xml, Catalogs/X.xml и т.п.).
/// Возвращает значение атрибута `uuid` первого дочернего элемента
/// `MetaDataObject`. None если структура нестандартная или uuid отсутствует.
pub fn extract_object_uuid_from_str(xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut depth = 0u32;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                depth += 1;
                if depth == 2 {
                    // первый дочерний элемент после <MetaDataObject>
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"uuid" {
                            return attr
                                .unescape_value()
                                .ok()
                                .map(|cow| cow.to_string());
                        }
                    }
                    return None;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
    None
}

/// Прочитать XML с диска и извлечь UUID объекта.
pub fn extract_object_uuid_from_file(path: &Path) -> Result<Option<String>> {
    let content = std::fs::read_to_string(path)?;
    Ok(extract_object_uuid_from_str(&content))
}

/// Извлечь UUID формы из `Form.xml` (`Forms/<FormName>/[Ext/]Form.xml`).
/// У форм uuid — атрибут самого корневого элемента `<Form>`, а не
/// дочернего как у обычных объектов.
pub fn extract_form_uuid_from_str(xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                // Любой первый element — это и есть корень <Form>.
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() == b"uuid" {
                        return attr
                            .unescape_value()
                            .ok()
                            .map(|cow| cow.to_string());
                    }
                }
                return None;
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
    None
}

/// Прочитать `Form.xml` с диска и извлечь uuid формы.
pub fn extract_form_uuid_from_file(path: &Path) -> Result<Option<String>> {
    let content = std::fs::read_to_string(path)?;
    Ok(extract_form_uuid_from_str(&content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_uuid_from_typical_document_xml() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
  <Document uuid="12f1d8bf-4a3c-4d51-9e0a-1234567890ab">
    <Properties><Name>X</Name></Properties>
  </Document>
</MetaDataObject>"#;
        assert_eq!(
            extract_object_uuid_from_str(xml).as_deref(),
            Some("12f1d8bf-4a3c-4d51-9e0a-1234567890ab")
        );
    }

    #[test]
    fn extract_uuid_from_catalog_xml() {
        let xml = r#"<?xml version="1.0"?>
<MetaDataObject><Catalog uuid="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee">
<Properties><Name>Контрагенты</Name></Properties></Catalog></MetaDataObject>"#;
        assert_eq!(
            extract_object_uuid_from_str(xml).as_deref(),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
        );
    }

    #[test]
    fn extract_uuid_returns_none_when_missing() {
        let xml = r#"<MetaDataObject><Document><Properties/></Document></MetaDataObject>"#;
        assert!(extract_object_uuid_from_str(xml).is_none());
    }

    #[test]
    fn extract_uuid_returns_none_for_empty() {
        assert!(extract_object_uuid_from_str("").is_none());
    }

    #[test]
    fn extract_uuid_does_not_panic_on_malformed_xml() {
        assert!(extract_object_uuid_from_str("<not xml at all").is_none());
        assert!(extract_object_uuid_from_str("<a><b><<<").is_none());
    }

    #[test]
    fn extract_form_uuid_from_root() {
        let xml = r#"<?xml version="1.0"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform"
      uuid="b4d8a4b7-1111-2222-3333-444444444444"
      title="Форма документа">
  <Events><Event name="ПриОткрытии">ПриОткрытии</Event></Events>
</Form>"#;
        assert_eq!(
            extract_form_uuid_from_str(xml).as_deref(),
            Some("b4d8a4b7-1111-2222-3333-444444444444")
        );
    }

    #[test]
    fn extract_form_uuid_returns_none_when_missing() {
        let xml = r#"<Form><Events/></Form>"#;
        assert!(extract_form_uuid_from_str(xml).is_none());
    }
}
