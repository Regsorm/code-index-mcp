// Парсеры XML-источников связей метаданных КОНФИГУРАЦИОННОГО уровня, которых
// нет в объектных XML реквизитов (их разбирает `object_attributes`). Дополняют
// граф данных `data_links` рёбрами «объект конфигурации → объект»:
//
//   * `subsystem_content`          — состав подсистемы (Subsystems/**.xml,
//     элемент `<Content><xr:Item>`); рекурсивно по вложенным подсистемам;
//   * `exchange_plan_content`      — состав плана обмена
//     (ExchangePlans/<X>/Ext/Content.xml, `<Item><Metadata>`);
//   * `defined_type_content`       — типы определяемого типа
//     (DefinedTypes/<X>.xml, `<Type><v8:Type>` через `classify_type`);
//   * `functional_option_location` — где хранится значение ФО
//     (FunctionalOptions/<X>.xml, `<Location>`).
//
// Права ролей (`Roles/<X>/Ext/Rights.xml`) выносятся в ОТДЕЛЬНУЮ таблицу
// `role_rights` (право — атрибут пары роль↔объект, а не ребро объект↔объект,
// поэтому не в data_links). Парсер прав — здесь же.
//
// Все парсеры event-based (quick_xml), в одном стиле с `object_attributes`,
// чтобы не тянуть DOM-дерево в память для крупных файлов (Content.xml /
// Rights.xml бывают по сотни КБ).

use anyhow::Result;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::path::Path;

use super::object_attributes::classify_type;

/// Имя тега без namespace-префикса (`xr:Item` → `Item`).
fn local_name(name: &str) -> String {
    match name.find(':') {
        Some(idx) => name[idx + 1..].to_string(),
        None => name.to_string(),
    }
}

/// Прочитать файл в строку, `Ok(None)` если файла нет.
fn read_to_string_opt(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(path)?))
}

// ── Подсистемы: <Content><xr:Item>Document.X</xr:Item> ─────────────────────

/// Распарсить XML подсистемы → список объектов её состава (канонические
/// `MetaType.Name`, как лежат в `<xr:Item>`). Пустой список, если состава нет.
pub fn parse_subsystem_content_xml(content: &str) -> Result<Vec<String>> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out: Vec<String> = Vec::new();
    // Состав живёт строго внутри <Content>; ChildObjects/прочее игнорируем.
    let mut in_content = false;
    let mut expect_item = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = local_name(&String::from_utf8_lossy(e.name().as_ref()));
                if local == "Content" {
                    in_content = true;
                } else if in_content && local == "Item" {
                    expect_item = true;
                }
            }
            Ok(Event::Text(t)) => {
                if expect_item {
                    let txt = t.unescape().map(|s| s.into_owned()).unwrap_or_default();
                    let txt = txt.trim().to_string();
                    if !txt.is_empty() {
                        out.push(txt);
                    }
                    expect_item = false;
                }
            }
            Ok(Event::End(e)) => {
                if local_name(&String::from_utf8_lossy(e.name().as_ref())) == "Content" {
                    in_content = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "subsystem XML: ошибка парсинга на позиции {}: {}",
                    reader.buffer_position(),
                    e
                ))
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

// ── Планы обмена: <Item><Metadata>Catalog.X</Metadata> ─────────────────────

/// Распарсить Content.xml плана обмена → список объектов состава
/// (`<Item><Metadata>`). `<AutoRecord>` не сохраняется (data_links хранит
/// только факт ребра, без атрибута авторегистрации).
pub fn parse_exchange_plan_content_xml(content: &str) -> Result<Vec<String>> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out: Vec<String> = Vec::new();
    let mut expect_meta = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                if local_name(&String::from_utf8_lossy(e.name().as_ref())) == "Metadata" {
                    expect_meta = true;
                }
            }
            Ok(Event::Text(t)) => {
                if expect_meta {
                    let txt = t.unescape().map(|s| s.into_owned()).unwrap_or_default();
                    let txt = txt.trim().to_string();
                    if !txt.is_empty() {
                        out.push(txt);
                    }
                    expect_meta = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "exchange plan content XML: ошибка парсинга на позиции {}: {}",
                    reader.buffer_position(),
                    e
                ))
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

// ── Определяемые типы: <Type><v8:Type>cfg:CatalogRef.X</v8:Type> ───────────

/// Распарсить XML определяемого типа → ссылочные цели (через `classify_type`),
/// дедуплицированные и отсортированные. `bool` — `is_universal` цели.
/// Контейнер `<Type>` отличается от элементов `<v8:Type>` по сырому имени
/// (как в `object_attributes`): `raw == "Type"` vs `raw.ends_with(":Type")`.
pub fn parse_defined_type_targets_xml(content: &str) -> Result<Vec<(String, bool)>> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut raw_types: Vec<String> = Vec::new();
    let mut in_type = false;
    let mut expect_value = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let raw = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if raw == "Type" {
                    in_type = true;
                } else if in_type && raw.ends_with(":Type") {
                    expect_value = true;
                }
            }
            Ok(Event::Text(t)) => {
                if expect_value {
                    let txt = t.unescape().map(|s| s.into_owned()).unwrap_or_default();
                    let txt = txt.trim().to_string();
                    if !txt.is_empty() {
                        raw_types.push(txt);
                    }
                    expect_value = false;
                }
            }
            Ok(Event::End(e)) => {
                if String::from_utf8_lossy(e.name().as_ref()) == "Type" {
                    in_type = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "defined type XML: ошибка парсинга на позиции {}: {}",
                    reader.buffer_position(),
                    e
                ))
            }
            _ => {}
        }
        buf.clear();
    }

    let mut targets: Vec<(String, bool)> =
        raw_types.iter().filter_map(|t| classify_type(t)).collect();
    targets.sort();
    targets.dedup();
    Ok(targets)
}

// ── Функциональные опции: <Location>InformationRegister.X.Resource.Y ───────

/// Извлечь объект хранения значения ФО из `<Location>`. Формат —
/// `<MetaType>.<Name>.<...>` (`Constant.X`, `InformationRegister.X.Resource.Y`);
/// объект = первые два сегмента (`Constant.X` / `InformationRegister.X`).
/// Возвращает `(object, raw_location)` либо `None`, если `<Location>` пуст.
pub fn parse_functional_option_location_xml(content: &str) -> Result<Option<(String, String)>> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut expect_loc = false;
    let mut raw_location: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                if local_name(&String::from_utf8_lossy(e.name().as_ref())) == "Location" {
                    expect_loc = true;
                }
            }
            Ok(Event::Text(t)) => {
                if expect_loc {
                    let txt = t.unescape().map(|s| s.into_owned()).unwrap_or_default();
                    let txt = txt.trim().to_string();
                    if !txt.is_empty() {
                        raw_location = Some(txt);
                    }
                    expect_loc = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "functional option XML: ошибка парсинга на позиции {}: {}",
                    reader.buffer_position(),
                    e
                ))
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(raw_location.and_then(|loc| {
        let mut it = loc.splitn(3, '.');
        match (it.next(), it.next()) {
            (Some(kind), Some(name)) if !kind.is_empty() && !name.is_empty() => {
                Some((format!("{}.{}", kind, name), loc))
            }
            _ => None,
        }
    }))
}

// ── Права ролей: <object><name>…</name><right><name>…</name><value>true ─────

/// Одно granted-право роли на объект.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleRight {
    /// Полное имя объекта (`Document.X`, `Configuration.Y`, `Catalog.Z`).
    pub object_name: String,
    /// Имя права (`Read`, `Insert`, `Posting`, `ThinClient`, …).
    pub right_name: String,
}

/// Распарсить Rights.xml роли → список granted-прав (только `<value>true</value>`).
/// Структура: `<object><name>Obj</name><right><name>R</name><value>true</value>…`.
/// `<name>` под `<object>` (вне `<right>`) — имя объекта; `<name>` под `<right>` —
/// имя права.
pub fn parse_role_rights_xml(content: &str) -> Result<Vec<RoleRight>> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out: Vec<RoleRight> = Vec::new();

    let mut in_object = false;
    let mut in_right = false;
    let mut cur_object: Option<String> = None;
    let mut cur_right: Option<String> = None;
    let mut cur_value: Option<String> = None;

    // Куда направить ближайший текстовый узел.
    #[derive(PartialEq)]
    enum T {
        None,
        ObjName,
        RightName,
        Value,
    }
    let mut tt = T::None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = local_name(&String::from_utf8_lossy(e.name().as_ref()));
                match local.as_str() {
                    "object" => {
                        in_object = true;
                        cur_object = None;
                    }
                    "right" if in_object => {
                        in_right = true;
                        cur_right = None;
                        cur_value = None;
                    }
                    "name" => {
                        if in_right {
                            tt = T::RightName;
                        } else if in_object {
                            tt = T::ObjName;
                        }
                    }
                    "value" if in_right => {
                        tt = T::Value;
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if tt != T::None {
                    let txt = t.unescape().map(|s| s.into_owned()).unwrap_or_default();
                    let txt = txt.trim().to_string();
                    match tt {
                        T::ObjName => {
                            if !txt.is_empty() {
                                cur_object = Some(txt);
                            }
                        }
                        T::RightName => {
                            if !txt.is_empty() {
                                cur_right = Some(txt);
                            }
                        }
                        T::Value => cur_value = Some(txt),
                        T::None => {}
                    }
                    tt = T::None;
                }
            }
            Ok(Event::End(e)) => {
                let local = local_name(&String::from_utf8_lossy(e.name().as_ref()));
                match local.as_str() {
                    "right" => {
                        if cur_value.as_deref() == Some("true") {
                            if let (Some(obj), Some(right)) =
                                (cur_object.as_ref(), cur_right.as_ref())
                            {
                                out.push(RoleRight {
                                    object_name: obj.clone(),
                                    right_name: right.clone(),
                                });
                            }
                        }
                        in_right = false;
                        cur_right = None;
                        cur_value = None;
                    }
                    "object" => {
                        in_object = false;
                        cur_object = None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "role rights XML: ошибка парсинга на позиции {}: {}",
                    reader.buffer_position(),
                    e
                ))
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

// ── Файловые обёртки ───────────────────────────────────────────────────────

pub fn parse_subsystem_content_file(path: &Path) -> Result<Vec<String>> {
    match read_to_string_opt(path)? {
        Some(c) => parse_subsystem_content_xml(&c),
        None => Ok(Vec::new()),
    }
}

pub fn parse_exchange_plan_content_file(path: &Path) -> Result<Vec<String>> {
    match read_to_string_opt(path)? {
        Some(c) => parse_exchange_plan_content_xml(&c),
        None => Ok(Vec::new()),
    }
}

pub fn parse_defined_type_targets_file(path: &Path) -> Result<Vec<(String, bool)>> {
    match read_to_string_opt(path)? {
        Some(c) => parse_defined_type_targets_xml(&c),
        None => Ok(Vec::new()),
    }
}

pub fn parse_functional_option_location_file(path: &Path) -> Result<Option<(String, String)>> {
    match read_to_string_opt(path)? {
        Some(c) => parse_functional_option_location_xml(&c),
        None => Ok(None),
    }
}

pub fn parse_role_rights_file(path: &Path) -> Result<Vec<RoleRight>> {
    match read_to_string_opt(path)? {
        Some(c) => parse_role_rights_xml(&c),
        None => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsystem_content_collects_items() {
        let xml = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:xr="x" xmlns:xsi="y">
  <Subsystem uuid="s1">
    <Properties>
      <Name>CRM</Name>
      <Content>
        <xr:Item xsi:type="xr:MDObjectRef">Document.РассылкаКлиентам</xr:Item>
        <xr:Item xsi:type="xr:MDObjectRef">Catalog.Претензии</xr:Item>
      </Content>
    </Properties>
    <ChildObjects/>
  </Subsystem>
</MetaDataObject>"#;
        let items = parse_subsystem_content_xml(xml).unwrap();
        assert_eq!(
            items,
            vec![
                "Document.РассылкаКлиентам".to_string(),
                "Catalog.Претензии".to_string()
            ]
        );
    }

    #[test]
    fn subsystem_without_content_empty() {
        let xml = r#"<MetaDataObject><Subsystem><Properties><Name>X</Name>
          <Content/></Properties><ChildObjects/></Subsystem></MetaDataObject>"#;
        assert!(parse_subsystem_content_xml(xml).unwrap().is_empty());
    }

    #[test]
    fn exchange_plan_content_collects_metadata() {
        let xml = r#"<?xml version="1.0"?>
<ExchangePlanContent xmlns="z">
  <Item><Metadata>Catalog.ВариантыОтчетов</Metadata><AutoRecord>Deny</AutoRecord></Item>
  <Item><Metadata>Catalog.Партнеры</Metadata><AutoRecord>Allow</AutoRecord></Item>
</ExchangePlanContent>"#;
        let items = parse_exchange_plan_content_xml(xml).unwrap();
        assert_eq!(
            items,
            vec![
                "Catalog.ВариантыОтчетов".to_string(),
                "Catalog.Партнеры".to_string()
            ]
        );
    }

    #[test]
    fn defined_type_targets_classified_and_deduped() {
        let xml = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="c">
  <DefinedType uuid="d1">
    <InternalInfo>
      <xr:GeneratedType name="DefinedType.X" category="DefinedType">
        <xr:TypeId>abc</xr:TypeId>
      </xr:GeneratedType>
    </InternalInfo>
    <Properties>
      <Name>X</Name>
      <Type>
        <v8:Type>cfg:CatalogRef.Пользователи</v8:Type>
        <v8:Type>cfg:CatalogRef.Пользователи</v8:Type>
        <v8:Type>cfg:EnumRef.ВидыДат</v8:Type>
        <v8:Type>xs:string</v8:Type>
      </Type>
    </Properties>
  </DefinedType>
</MetaDataObject>"#;
        let t = parse_defined_type_targets_xml(xml).unwrap();
        // Дедуп Пользователи, примитив xs:string отброшен, GeneratedType не тип.
        assert_eq!(
            t,
            vec![
                ("Catalog.Пользователи".to_string(), false),
                ("Enum.ВидыДат".to_string(), false),
            ]
        );
    }

    #[test]
    fn functional_option_location_parsed() {
        let xml = r#"<MetaDataObject><FunctionalOption><Properties>
          <Name>ВариантыВерсионированияОбъектов</Name>
          <Location>InformationRegister.НастройкиВерсионированияОбъектов.Resource.Вариант</Location>
          <Content/></Properties></FunctionalOption></MetaDataObject>"#;
        let r = parse_functional_option_location_xml(xml).unwrap().unwrap();
        assert_eq!(r.0, "InformationRegister.НастройкиВерсионированияОбъектов");
        assert!(r.1.ends_with("Resource.Вариант"));
    }

    #[test]
    fn functional_option_constant_location() {
        let xml = r#"<MetaDataObject><FunctionalOption><Properties>
          <Location>Constant.ИспользоватьСкладскойУчет</Location>
          </Properties></FunctionalOption></MetaDataObject>"#;
        let r = parse_functional_option_location_xml(xml).unwrap().unwrap();
        assert_eq!(r.0, "Constant.ИспользоватьСкладскойУчет");
    }

    #[test]
    fn functional_option_empty_location() {
        let xml = r#"<MetaDataObject><FunctionalOption><Properties>
          <Location/></Properties></FunctionalOption></MetaDataObject>"#;
        assert!(parse_functional_option_location_xml(xml).unwrap().is_none());
    }

    #[test]
    fn role_rights_only_granted() {
        let xml = r#"<?xml version="1.0"?>
<Rights xmlns="r">
  <setForNewObjects>false</setForNewObjects>
  <object>
    <name>Document.РеализацияТоваровУслуг</name>
    <right><name>Read</name><value>true</value></right>
    <right><name>Insert</name><value>false</value></right>
    <right><name>Posting</name><value>true</value></right>
  </object>
  <object>
    <name>Catalog.Контрагенты</name>
    <right><name>Read</name><value>true</value></right>
  </object>
</Rights>"#;
        let rr = parse_role_rights_xml(xml).unwrap();
        assert_eq!(
            rr,
            vec![
                RoleRight {
                    object_name: "Document.РеализацияТоваровУслуг".to_string(),
                    right_name: "Read".to_string()
                },
                RoleRight {
                    object_name: "Document.РеализацияТоваровУслуг".to_string(),
                    right_name: "Posting".to_string()
                },
                RoleRight {
                    object_name: "Catalog.Контрагенты".to_string(),
                    right_name: "Read".to_string()
                },
            ]
        );
    }
}
