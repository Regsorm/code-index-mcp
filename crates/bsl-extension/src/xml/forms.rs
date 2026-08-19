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
    /// Элемент формы, которому принадлежит обработчик (`СуммаДокумента`,
    /// `ТоварыКоличество`). `None` — обработчик самой формы, а не элемента.
    /// Имена элементов уникальны в пределах формы, поэтому путь вложенности
    /// не хранится: к элементу обращаются как `Элементы.<Имя>`.
    pub element: Option<String>,
}

/// Нормализует имя события ФОРМЫ к русскому виду (как в конфигураторе).
///
/// В выгрузке — и в формате Конфигуратора, и в EDT — события формы и её
/// элементов записаны английскими идентификаторами платформы (`OnChange`,
/// `OnCreateAtServer`); проверено на пяти базах: русских имён нет ни одного.
/// Разработчик же видит в конфигураторе русские названия, и таблица подписок
/// (`event_subscriptions`) тоже хранит русские — поэтому переводим и здесь,
/// чтобы выдача была единообразной.
///
/// Словарь событий ПОДПИСОК (`event_subscriptions::event_to_russian`) для этого
/// не годится: он собран под события объектов и с событиями форм пересекается
/// всего в нескольких именах — от него получалась мешанина, где `ПередЗаписью`
/// стоит рядом с непереведённым `BeforeWriteAtServer`.
///
/// Неизвестные имена возвращаются как есть — ничего не теряется. Так же
/// проходят идентификаторы событий внешних компонент (`9cc34712-da5f-…`),
/// которым русского имени не существует.
///
/// Соответствия не выдуманы: каждое сверено по самим конфигурациям. Обработчики
/// в 1С называют по образцу «ИмяЭлемента + ИмяСобытия»
/// (`СуммаДокументаПриИзменении`), поэтому русское имя события читается из
/// хвоста имени процедуры. Проверка «сколько обработчиков события кончаются на
/// предполагаемое имя» прогнана по четырём типовым конфигурациям; она же
/// поправила два неверных перевода (`EditTextChange` — без приставки «При»,
/// `AdditionalDetailProcessing` — другой порядок слов).
pub fn form_event_to_russian(event: &str) -> &str {
    match event {
        // ── События самой формы ──────────────────────────────────────────
        "OnOpen" => "ПриОткрытии",
        "OnClose" => "ПриЗакрытии",
        "BeforeClose" => "ПередЗакрытием",
        "OnCreateAtServer" => "ПриСозданииНаСервере",
        "OnReadAtServer" => "ПриЧтенииНаСервере",
        "BeforeWrite" => "ПередЗаписью",
        "BeforeWriteAtServer" => "ПередЗаписьюНаСервере",
        "OnWriteAtServer" => "ПриЗаписиНаСервере",
        "AfterWrite" => "ПослеЗаписи",
        "AfterWriteAtServer" => "ПослеЗаписиНаСервере",
        "FillCheckProcessingAtServer" => "ОбработкаПроверкиЗаполненияНаСервере",
        "NotificationProcessing" => "ОбработкаОповещения",
        "ChoiceProcessing" => "ОбработкаВыбора",
        "NewWriteProcessing" => "ОбработкаЗаписиНового",
        "OnReopen" => "ПриПовторномОткрытии",
        "ExternalEvent" => "ВнешнееСобытие",
        "URLProcessing" => "ОбработкаНавигационнойСсылки",
        "OnSaveDataInSettingsAtServer" => "ПриСохраненииДанныхВНастройкахНаСервере",
        "OnLoadDataFromSettingsAtServer" => "ПриЗагрузкеДанныхИзНастроекНаСервере",
        "BeforeLoadDataFromSettingsAtServer" => "ПередЗагрузкойДанныхИзНастроекНаСервере",
        // ── События элементов формы ──────────────────────────────────────
        "OnChange" => "ПриИзменении",
        "StartChoice" => "НачалоВыбора",
        "StartListChoice" => "НачалоВыбораИзСписка",
        "Clearing" => "Очистка",
        "Opening" => "Открытие",
        "AutoComplete" => "АвтоПодбор",
        "TextEditEnd" => "ОкончаниеВводаТекста",
        "EditTextChange" => "ИзменениеТекстаРедактирования",
        "Click" => "Нажатие",
        "Selection" => "Выбор",
        "ValueChoice" => "ВыборЗначения",
        "BeforeAddRow" => "ПередНачаломДобавления",
        "BeforeRowChange" => "ПередНачаломИзменения",
        "BeforeDeleteRow" => "ПередУдалением",
        "AfterDeleteRow" => "ПослеУдаления",
        "OnStartEdit" => "ПриНачалеРедактирования",
        "OnEditEnd" => "ПриОкончанииРедактирования",
        "BeforeEditEnd" => "ПередОкончаниемРедактирования",
        "OnActivateRow" => "ПриАктивизацииСтроки",
        "OnActivateCell" => "ПриАктивизацииЯчейки",
        "OnActivateField" => "ПриАктивизацииПоля",
        "OnActivate" => "ПриАктивизации",
        "Drag" => "Перетаскивание",
        "DragStart" => "НачалоПеретаскивания",
        "DragEnd" => "ОкончаниеПеретаскивания",
        "DragCheck" => "ПроверкаПеретаскивания",
        "BeforeExpand" => "ПередРазворачиванием",
        "BeforeCollapse" => "ПередСворачиванием",
        "OnCurrentPageChange" => "ПриСменеСтраницы",
        "OnGetDataAtServer" => "ПриПолученииДанныхНаСервере",
        "DetailProcessing" => "ОбработкаРасшифровки",
        "AdditionalDetailProcessing" => "ОбработкаДополнительнойРасшифровки",
        "DocumentComplete" => "ДокументСформирован",
        "OnClick" => "ПриНажатии",
        "Creating" => "Создание",
        // ── Расширение формы отчёта ──────────────────────────────────────
        "OnUpdateUserSettingSetAtServer" => "ПриОбновленииСоставаПользовательскихНастроекНаСервере",
        "OnSaveUserSettingsAtServer" => "ПриСохраненииПользовательскихНастроекНаСервере",
        "OnLoadUserSettingsAtServer" => "ПриЗагрузкеПользовательскихНастроекНаСервере",
        "BeforeLoadUserSettingsAtServer" => "ПередЗагрузкойПользовательскихНастроекНаСервере",
        "OnSaveVariantAtServer" => "ПриСохраненииВариантаНаСервере",
        "OnLoadVariantAtServer" => "ПриЗагрузкеВариантаНаСервере",
        "BeforeLoadVariantAtServer" => "ПередЗагрузкойВариантаНаСервере",
        "BeforePrint" => "ПередПечатью",
        // ── Редкие события: таблица, календарь, дерево, планировщик,
        //    поле HTML-документа, система взаимодействия ────────────────────
        "Tuning" => "Регулирование",
        "RefreshRequestProcessing" => "ОбработкаЗапросаОбновления",
        "URLGetProcessing" => "ОбработкаПолученияНавигационнойСсылки",
        "URLListGetProcessing" => "ОбработкаПолученияСпискаНавигационныхСсылок",
        "NavigationProcessing" => "ОбработкаПерехода",
        "ActivationProcessing" => "ОбработкаАктивизации",
        "OnChangeAreaContent" => "ПриИзмененииСодержимогоОбласти",
        "OnPeriodOutput" => "ПриВыводеПериода",
        "OnActivateDate" => "ПриАктивизацииДаты",
        "OnCurrentParentChange" => "ПриСменеТекущегоРодителя",
        "OnCurrentRepresentationPeriodChange" => "ПриСменеТекущегоПериодаОтображения",
        "MultipleValueOpening" => "ОткрытиеМножественногоЗначения",
        "MultipleValuesDelete" => "УдалениеМножественныхЗначений",
        "BeforeStartEdit" => "ПередНачаломРедактирования",
        "BeforeStartQuickEdit" => "ПередНачаломБыстрогоРедактирования",
        "BeforeCreate" => "ПередСозданием",
        "BeforeDelete" => "ПередУдалением",
        "BeforeExecute" => "ПередВыполнением",
        "CommandGenerateProcessing" => "ОбработкаФормированияКоманд",
        "OnMainServerAvailabilityChange" => "ПриИзмененииДоступностиОсновногоСервера",
        "OnClientApplicationSuspend" => "ПриЗасыпанииКлиентскогоПриложения",
        "OnClientApplicationResume" => "ПриПробужденииКлиентскогоПриложения",
        "OnReopenFromOtherServer" => "ПриПереоткрытииСДругогоСервера",
        "BeforeReopenFromOtherServer" => "ПередПереоткрытиемСДругогоСервера",
        "AddInDetachmentOnError" => "ОтключениеВнешнейКомпонентыПриОшибке",
        "OnChangeDisplaySettings" => "ПриИзмененииПараметровЭкрана",
        "CollaborationSystemUsersAutoComplete" => "АвтоПодборПользователейСистемыВзаимодействия",
        "CollaborationSystemUsersChoiceFormGetProcessing" => {
            "ОбработкаПолученияФормыВыбораПользователейСистемыВзаимодействия"
        }
        other => other,
    }
}

/// Сериализовать обработчики для колонки `metadata_forms.handlers_json`.
/// Формат общий для обоих форматов выгрузки; `element` опускается у
/// обработчиков самой формы.
pub fn handlers_to_json(handlers: &[FormHandler]) -> Result<String> {
    let arr: Vec<serde_json::Value> = handlers
        .iter()
        .map(|h| match &h.element {
            Some(el) => serde_json::json!({
                "event": h.event,
                "handler": h.handler,
                "element": el,
            }),
            None => serde_json::json!({"event": h.event, "handler": h.handler}),
        })
        .collect();
    Ok(serde_json::to_string(&arr)?)
}

/// Распарсить XML-описание формы.
pub fn parse_form_xml(content: &str) -> Result<Vec<FormHandler>> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    let mut buf = Vec::new();
    let mut current_event_name: Option<String> = None;
    let mut tag_stack: Vec<String> = Vec::new();
    // Владельцы обработчиков: по элементу формы на каждый открытый тег с
    // атрибутом `name` (`<InputField name="Товар">`, `<Table name="Товары">`).
    // Владелец обработчика — ближайший такой предок; пусто — сама форма.
    let mut owner_stack: Vec<Option<String>> = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = local_name(e.name().as_ref());
                // Атрибут `name` есть и у тега события (там это имя события),
                // и у тегов элементов формы (там это имя элемента).
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
                if local == "Event" {
                    current_event_name = name_value;
                    owner_stack.push(None);
                } else {
                    owner_stack.push(name_value.filter(|v| !v.is_empty()));
                }
                tag_stack.push(local);
            }
            Ok(Event::End(e)) => {
                let local = local_name(e.name().as_ref());
                if local == "Event" {
                    current_event_name = None;
                }
                tag_stack.pop();
                owner_stack.pop();
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
                            // Ближайший предок с именем — сам тег `Event`
                            // владельцем не считается (он уже вытолкнут в None).
                            let element = owner_stack.iter().rev().find_map(|o| o.clone());
                            out.push(FormHandler {
                                event: form_event_to_russian(event_name).to_string(),
                                handler: handler_name,
                                element,
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
    fn handler_of_form_element_keeps_owner() {
        // Раскладка как в реальной выгрузке: поле лежит внутри нескольких
        // групп оформления, обработчики формы — в корневом <Events>.
        let xml = r#"<?xml version="1.0"?>
<Form>
  <Events>
    <Event name="OnOpen">ПриОткрытии</Event>
  </Events>
  <ChildItems>
    <Page name="ГруппаТовары" id="18">
      <ChildItems>
        <InputField name="СуммаДокумента" id="1312">
          <ContextMenu name="СуммаДокументаКонтекстноеМеню" id="1313"/>
          <Events>
            <Event name="OnChange">СуммаДокументаПриИзменении</Event>
          </Events>
        </InputField>
      </ChildItems>
    </Page>
  </ChildItems>
</Form>
"#;
        let handlers = parse_form_xml(xml).unwrap();
        assert_eq!(handlers.len(), 2);
        let form_level = handlers.iter().find(|h| h.event == "ПриОткрытии").unwrap();
        assert_eq!(form_level.element, None);
        let field = handlers.iter().find(|h| h.event == "ПриИзменении").unwrap();
        // Владелец — само поле, а не страница-контейнер.
        assert_eq!(field.element.as_deref(), Some("СуммаДокумента"));
    }

    #[test]
    fn form_events_translated_in_both_dump_formats() {
        // Имя события переводится, имя процедуры остаётся как в модуле.
        let xml = r#"<?xml version="1.0"?>
<Form>
  <Events>
    <Event name="OnCreateAtServer">ПриСозданииНаСервере</Event>
    <Event name="БезымянноеСобытие">Обработчик</Event>
  </Events>
</Form>
"#;
        let handlers = parse_form_xml(xml).unwrap();
        assert!(handlers.iter().any(|h| h.event == "ПриСозданииНаСервере"));
        // Неизвестное имя проходит без изменений — ничего не теряем.
        assert!(handlers.iter().any(|h| h.event == "БезымянноеСобытие"));
        // Идентификатор события внешней компоненты русского имени не имеет.
        assert_eq!(
            form_event_to_russian("9cc34712-da5f-4faa-a653-343d2085fbe8"),
            "9cc34712-da5f-4faa-a653-343d2085fbe8"
        );
        // Имена, сверенные по конфигурациям: приставки «При» у события
        // изменения текста нет, у дополнительной расшифровки — свой порядок
        // слов, а «Tuning» в конфигураторе называется «Регулирование».
        assert_eq!(
            form_event_to_russian("EditTextChange"),
            "ИзменениеТекстаРедактирования"
        );
        assert_eq!(
            form_event_to_russian("AdditionalDetailProcessing"),
            "ОбработкаДополнительнойРасшифровки"
        );
        assert_eq!(form_event_to_russian("Tuning"), "Регулирование");
    }

    #[test]
    fn handlers_json_omits_element_for_form_level() {
        let handlers = vec![
            FormHandler {
                event: "OnOpen".into(),
                handler: "ПриОткрытии".into(),
                element: None,
            },
            FormHandler {
                event: "OnChange".into(),
                handler: "СуммаПриИзменении".into(),
                element: Some("Сумма".into()),
            },
        ];
        let json = handlers_to_json(&handlers).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert!(parsed[0].get("element").is_none());
        assert_eq!(parsed[1]["element"], "Сумма");
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
