// Парсер XML-описаний управляемых форм 1С (`*/Forms/<Имя>/Form.xml`
// или `*/Forms/<Имя>/Ext/Form.xml` в зависимости от выгрузки).
//
// Назначение — извлечь обработчики событий формы. На уровне XML
// это выглядит так:
//
//   <Form>
//     <Events>
//       <Event name="ПриОткрытии">ПриОткрытии</Event>
//       <Event name="ПередЗакрытием">ПередЗакрытиемОбработчик</Event>
//     </Events>
//   </Form>
//
// `name` — имя события платформы 1С, текстовое содержимое тега —
// имя процедуры в модуле формы. Они часто совпадают, но БСП-расширения
// могут проксировать стандартные события на свои обработчики, тогда
// имена расходятся.
//
// Реальные дампы 1С могут отличаться по namespace и обёрткам; парсер
// делает мягкое сопоставление по local-имени тега, без жёсткой
// привязки к конкретной структуре XML — это позволяет обрабатывать
// и DumpConfigToFiles, и v8unpack-выгрузку, и форматы расширений.

use std::path::Path;

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

/// Один обработчик события формы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormHandler {
    /// Имя события платформы 1С: `ПриОткрытии`, `ПередЗакрытием`, `ПриСозданииНаСервере`...
    pub event: String,
    /// Имя процедуры в модуле формы, на которую назначен обработчик.
    pub handler: String,
}

/// Распарсить XML-описание формы.
pub fn parse_form_xml(content: &str) -> Result<Vec<FormHandler>> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    let mut buf = Vec::new();
    let mut current_event_name: Option<String> = None;
    let mut tag_stack: Vec<String> = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = local_name(e.name().as_ref());
                if local == "Event" {
                    // Имя события приходит атрибутом `name="..."`.
                    let mut name_value: Option<String> = None;
                    for attr in e.attributes().with_checks(false) {
                        if let Ok(a) = attr {
                            if local_name(a.key.as_ref()) == "name" {
                                let v = a
                                    .unescape_value()
                                    .map(|s| s.into_owned())
                                    .unwrap_or_default();
                                name_value = Some(v);
                            }
                        }
                    }
                    current_event_name = name_value;
                }
                tag_stack.push(local);
            }
            Ok(Event::End(e)) => {
                let local = local_name(e.name().as_ref());
                if local == "Event" {
                    current_event_name = None;
                }
                tag_stack.pop();
            }
            Ok(Event::Text(text)) => {
                let parent = tag_stack.last().map(|s| s.as_str()).unwrap_or("");
                if parent == "Event" {
                    if let Some(event_name) = &current_event_name {
                        let handler_name = text
                            .unescape()
                            .map(|s| s.into_owned())
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        if !handler_name.is_empty() && !event_name.is_empty() {
                            out.push(FormHandler {
                                event: event_name.clone(),
                                handler: handler_name,
                            });
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Form XML: ошибка парсинга на позиции {}: {}",
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

/// Прочитать XML формы по пути. Возвращает пустой Vec если файла нет —
/// форма может быть закодирована в другом формате выгрузки.
pub fn parse_form_file(path: &Path) -> Result<Vec<FormHandler>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Не удалось прочитать {}", path.display()))?;
    parse_form_xml(&content)
}

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
<Form>
  <Properties>
    <Title><v8:item lang="ru"><v8:content>Форма документа</v8:content></v8:item></Title>
  </Properties>
  <Events>
    <Event name="ПриОткрытии">ПриОткрытии</Event>
    <Event name="ПередЗакрытием">ПередЗакрытиемОбработчик</Event>
    <Event name="ПриСозданииНаСервере">ПриСозданииНаСервере</Event>
  </Events>
</Form>
"#;

    #[test]
    fn parses_three_handlers() {
        let handlers = parse_form_xml(SAMPLE).unwrap();
        assert_eq!(handlers.len(), 3);
    }

    #[test]
    fn handler_with_renamed_proc() {
        let handlers = parse_form_xml(SAMPLE).unwrap();
        let renamed = handlers
            .iter()
            .find(|h| h.event == "ПередЗакрытием")
            .unwrap();
        assert_eq!(renamed.handler, "ПередЗакрытиемОбработчик");
    }

    #[test]
    fn ignores_text_outside_event_tag() {
        // <v8:content>Форма документа</v8:content> внутри <Title> не
        // должно попасть в обработчики событий.
        let handlers = parse_form_xml(SAMPLE).unwrap();
        assert!(!handlers.iter().any(|h| h.handler.contains("Форма")));
    }

    #[test]
    fn returns_empty_for_missing_file() {
        let p = std::path::Path::new("/non/existent.xml");
        assert!(parse_form_file(p).unwrap().is_empty());
    }

    #[test]
    fn empty_events_block_yields_empty_vec() {
        let xml = r#"<?xml version="1.0"?>
<Form>
  <Events />
</Form>
"#;
        assert!(parse_form_xml(xml).unwrap().is_empty());
    }
}
