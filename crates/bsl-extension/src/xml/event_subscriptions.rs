// Парсер `EventSubscriptions/<Name>.xml` — XML-описаний подписок на события 1С.
//
// Подписка связывает событие платформы (`ПриЗаписи` у документа) с
// процедурой общего модуля. Пример:
//
//   <MetaDataObject>
//     <EventSubscription>
//       <Properties>
//         <Name>ОбновлениеСрезаПриЗаписи</Name>
//         <Source>
//           <Type>
//             <v8:Type>cfg:DocumentRef.РеализацияТоваровУслуг</v8:Type>
//             <v8:Type>cfg:DocumentRef.ПоступлениеТоваровУслуг</v8:Type>
//           </Type>
//         </Source>
//         <Event>ПриЗаписи</Event>
//         <Handler>ОбновлениеСреза.ПриЗаписиДокумента</Handler>
//       </Properties>
//     </EventSubscription>
//   </MetaDataObject>
//
// Из этого мы извлекаем:
// - имя подписки (для UNIQUE-ключа в БД),
// - событие,
// - handler в формате `Module.Procedure` → разбиваем на module/proc,
// - список источников — типы, к которым привязана подписка.

use std::path::Path;

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

/// Описание одной подписки на событие.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventSubscription {
    /// Имя подписки (`Name` из XML).
    pub name: String,
    /// Событие платформы (`Event`): `ПриЗаписи`, `ПередЗаписью`, ...
    pub event: String,
    /// Имя общего модуля, в котором лежит обработчик.
    /// Извлекается из `Handler` = `Module.Procedure` — это всё до последней точки.
    pub handler_module: String,
    /// Имя процедуры — всё после последней точки.
    pub handler_proc: String,
    /// Источники: типы объектов, к которым привязана подписка.
    /// Полные строки как в XML (`cfg:DocumentRef.РеализацияТоваровУслуг`).
    pub sources: Vec<String>,
}

/// Распарсить XML-описание подписки.
pub fn parse_event_subscription_xml(content: &str) -> Result<Option<EventSubscription>> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut tag_stack: Vec<String> = Vec::new();
    let mut name: Option<String> = None;
    let mut event_value: Option<String> = None;
    let mut handler_value: Option<String> = None;
    let mut sources: Vec<String> = Vec::new();
    let mut in_subscription = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = local_name(e.name().as_ref());
                if local == "EventSubscription" {
                    in_subscription = true;
                }
                tag_stack.push(local);
            }
            Ok(Event::End(e)) => {
                let local = local_name(e.name().as_ref());
                if local == "EventSubscription" {
                    in_subscription = false;
                }
                tag_stack.pop();
            }
            Ok(Event::Text(text)) => {
                if !in_subscription {
                    continue;
                }
                let parent = tag_stack.last().map(|s| s.as_str()).unwrap_or("");
                let value = text
                    .unescape()
                    .map(|s| s.into_owned())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if value.is_empty() {
                    continue;
                }
                match parent {
                    "Name" => {
                        // <Name> внутри <Properties>, не глобальный `<Name>` другого уровня.
                        if tag_stack.iter().any(|t| t == "Properties") && name.is_none() {
                            name = Some(value);
                        }
                    }
                    "Event" => {
                        if tag_stack.iter().any(|t| t == "Properties") {
                            event_value = Some(value);
                        }
                    }
                    "Handler" => {
                        handler_value = Some(value);
                    }
                    "Type" => {
                        // <v8:Type>cfg:DocumentRef.РеализацияТоваровУслуг</v8:Type>
                        // — внутри <Source>/<Type>/<v8:Type>. Парсим все вхождения.
                        if tag_stack.iter().any(|t| t == "Source") {
                            sources.push(value);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "EventSubscription XML: ошибка парсинга на позиции {}: {}",
                    reader.buffer_position(),
                    e
                ));
            }
            _ => {}
        }
        buf.clear();
    }

    let (Some(name), Some(event), Some(handler)) = (name, event_value, handler_value) else {
        return Ok(None);
    };

    // Handler разбиваем по последней точке. Если точки нет — handler_module
    // пустой; такая подписка некорректна, но мы её не валим — caller увидит
    // пустой module и решит сам, как реагировать (как правило, warn).
    let (module, proc_) = match handler.rfind('.') {
        Some(idx) => (handler[..idx].to_string(), handler[idx + 1..].to_string()),
        None => (String::new(), handler),
    };

    Ok(Some(EventSubscription {
        name,
        event,
        handler_module: module,
        handler_proc: proc_,
        sources,
    }))
}

/// Прочитать XML подписки по пути.
pub fn parse_event_subscription_file(path: &Path) -> Result<Option<EventSubscription>> {
    if !path.is_file() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Не удалось прочитать {}", path.display()))?;
    parse_event_subscription_xml(&content)
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
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <EventSubscription uuid="abc">
    <Properties>
      <Name>ОбновлениеСрезаПриЗаписи</Name>
      <Source>
        <Type>
          <v8:Type>cfg:DocumentRef.РеализацияТоваровУслуг</v8:Type>
          <v8:Type>cfg:DocumentRef.ПоступлениеТоваровУслуг</v8:Type>
        </Type>
      </Source>
      <Event>ПриЗаписи</Event>
      <Handler>ОбновлениеСреза.ПриЗаписиДокумента</Handler>
    </Properties>
  </EventSubscription>
</MetaDataObject>
"#;

    #[test]
    fn parses_full_subscription() {
        let sub = parse_event_subscription_xml(SAMPLE).unwrap().unwrap();
        assert_eq!(sub.name, "ОбновлениеСрезаПриЗаписи");
        assert_eq!(sub.event, "ПриЗаписи");
        assert_eq!(sub.handler_module, "ОбновлениеСреза");
        assert_eq!(sub.handler_proc, "ПриЗаписиДокумента");
        assert_eq!(sub.sources.len(), 2);
        assert!(sub
            .sources
            .iter()
            .any(|s| s.contains("РеализацияТоваровУслуг")));
    }

    #[test]
    fn returns_none_for_xml_without_subscription_block() {
        let xml = r#"<?xml version="1.0"?><MetaDataObject></MetaDataObject>"#;
        assert!(parse_event_subscription_xml(xml).unwrap().is_none());
    }

    #[test]
    fn handler_without_dot_yields_empty_module() {
        let xml = r#"<?xml version="1.0"?>
<MetaDataObject>
  <EventSubscription>
    <Properties>
      <Name>X</Name>
      <Event>E</Event>
      <Handler>BareName</Handler>
    </Properties>
  </EventSubscription>
</MetaDataObject>
"#;
        let sub = parse_event_subscription_xml(xml).unwrap().unwrap();
        assert_eq!(sub.handler_module, "");
        assert_eq!(sub.handler_proc, "BareName");
    }
}
