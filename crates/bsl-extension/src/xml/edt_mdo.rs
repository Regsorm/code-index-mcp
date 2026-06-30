// Парсер формата 1C:EDT (`.mdo`) — аналог парсеров формата Конфигуратора
// (configuration.rs / object_attributes.rs), но под раскладку и схему EDT.
//
// Отличия EDT от выгрузки Конфигуратора (DumpConfigToFiles):
//   * объект лежит в ПОДКАТАЛОГЕ: `Catalogs/<Имя>/<Имя>.mdo` (а не `Catalogs/<Имя>.xml`);
//   * корневой тег — `mdclass:Catalog` (ns `g5.1c.ru/v8/dt/metadata/mdclass`),
//     а не `MetaDataObject><Catalog`;
//   * теги camelCase: `<attributes>`/`<dimensions>`/`<resources>`/`<tabularSections>`/
//     `<enumValues>`/`<type>`/`<types>`/`<name>`/`<synonym>`/`<fillChecking>`;
//   * реквизит БЕЗ обёртки `<Properties>`; ТЧ-реквизиты — это `<attributes>`
//     ВНУТРИ `<tabularSections>`;
//   * синоним — `<synonym><key>ru</key><value>...</value></synonym>` (рядом
//     `<toolTip>` с такой же формой — берём value только из synonym);
//   * тип в платформенной нотации БЕЗ префикса `cfg:`: `CatalogRef.Номенклатура`,
//     `String`, `Number`, `Date`, `Boolean`, `DefinedType.X`, `AnyRef`;
//   * движения документа — `<registerRecords>AccumulationRegister.X</registerRecords>`
//     (значение уже каноническое имя регистра).
//
// Результат пишется в ТЕ ЖЕ структуры (ObjectStructure / DataLinkEdge) и таблицы,
// что и формат Конфигуратора — поэтому все downstream-инструменты
// (get_object_structure / get_data_links / get_register_writers / find_references /
// get_object_profile / bsl_sql) работают без изменений.

use anyhow::Result;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use super::object_attributes::{
    classify_type, pretty_types, DataLinkEdge, ObjectStructure, StructField, StructTabular,
};

/// Имя тега без namespace-префикса (`mdclass:Catalog` → `Catalog`).
fn local_name(name: &str) -> &str {
    match name.rfind(':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

/// Нормализовать тип `.mdo` (платформенная нотация без `cfg:`) в вид, который
/// понимают существующие `pretty_one_type` / `classify_type` (`cfg:`/`xs:`).
/// Так EDT-парсер переиспользует и человекочитаемое имя типа (русская нотация
/// для `attributes_json`), и классификацию ссылок (для `data_links`).
pub(crate) fn edt_type_to_cfg(t: &str) -> String {
    let t = t.trim();
    match t {
        "String" => return "xs:string".to_string(),
        "Number" => return "xs:decimal".to_string(),
        "Boolean" => return "xs:boolean".to_string(),
        "Date" | "DateTime" | "Time" => return "xs:dateTime".to_string(),
        _ => {}
    }
    // Ссылочные и определяемые типы → префикс `cfg:` (classify_type/pretty_one_type
    // ждут именно его). `CatalogRef.X`, `DocumentRef.X`, `EnumRef.X`, `AnyRef`,
    // `DefinedType.X`, обобщённый `CatalogRef` (без имени).
    if t == "AnyRef" || t.ends_with("Ref") || t.contains("Ref.") || t.starts_with("DefinedType.")
    {
        return format!("cfg:{}", t);
    }
    // Прочие платформенные типы (UUID, ValueStorage, ...) — как есть: для
    // структуры покажутся как есть, для data_links classify_type вернёт None.
    t.to_string()
}

/// Привести имя свойства из EDT-нотации (camelCase: `realTimePosting`) к
/// PascalCase Конфигуратора (`RealTimePosting`) — чтобы ключи секций
/// posting/properties совпадали с форматом выгрузки.
fn cap_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[derive(PartialEq, Clone, Copy)]
enum FieldKind {
    Attribute,
    TabAttr,
    Dimension,
    Resource,
    EnumValue,
    Command,
}

struct FieldBuild {
    kind: FieldKind,
    name: Option<String>,
    types: Vec<String>,
    synonym: Option<String>,
    required: bool,
}

impl FieldBuild {
    fn new(kind: FieldKind) -> Self {
        FieldBuild {
            kind,
            name: None,
            types: Vec::new(),
            synonym: None,
            required: false,
        }
    }
}

/// Распарсить `.mdo` объекта в полную структуру (реквизиты/ТЧ/измерения/ресурсы/
/// значения перечисления/свойства проведения/команды) — для `attributes_json`.
pub fn parse_mdo_structure_xml(content: &str) -> Result<ObjectStructure> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);
    let mut out = ObjectStructure::default();
    let mut buf = Vec::new();

    #[derive(PartialEq)]
    enum T {
        None,
        FieldName,
        TabName,
        TypeValue,
        SynValue,
        FillChecking,
        PostingProp,
        HeaderProp,
    }

    let mut in_std = false;
    let mut in_tabular = false;
    let mut cur_tab: Option<usize> = None;
    let mut expecting_tab_name = false;
    let mut field: Option<FieldBuild> = None;
    let mut in_type = false;
    let mut in_synonym = false;
    let mut tt = T::None;
    let mut cur_posting_prop: Option<String> = None;
    let mut cur_header_prop: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let raw = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let local = local_name(&raw).to_string();
                if local == "standardAttributes" {
                    in_std = true;
                    buf.clear();
                    continue;
                }
                if in_std {
                    buf.clear();
                    continue;
                }
                match local.as_str() {
                    "tabularSections" => {
                        in_tabular = true;
                        expecting_tab_name = true;
                        out.tabular_sections.push(StructTabular {
                            name: String::new(),
                            attributes: Vec::new(),
                        });
                        cur_tab = Some(out.tabular_sections.len() - 1);
                    }
                    "attributes" => {
                        field = Some(FieldBuild::new(if in_tabular {
                            FieldKind::TabAttr
                        } else {
                            FieldKind::Attribute
                        }));
                    }
                    "dimensions" => field = Some(FieldBuild::new(FieldKind::Dimension)),
                    "resources" => field = Some(FieldBuild::new(FieldKind::Resource)),
                    "enumValues" => field = Some(FieldBuild::new(FieldKind::EnumValue)),
                    "commands" => field = Some(FieldBuild::new(FieldKind::Command)),
                    "name" => {
                        if let Some(f) = field.as_ref() {
                            if f.name.is_none() {
                                tt = T::FieldName;
                            }
                        } else if expecting_tab_name {
                            tt = T::TabName;
                        }
                    }
                    "synonym" => {
                        if field.is_some() {
                            in_synonym = true;
                        }
                    }
                    "type" => {
                        if field.is_some() {
                            in_type = true;
                        }
                    }
                    "types" => {
                        if field.is_some() && in_type {
                            tt = T::TypeValue;
                        }
                    }
                    "value" => {
                        if field.is_some() && in_synonym {
                            tt = T::SynValue;
                        }
                    }
                    "fillChecking" => {
                        if field.is_some() {
                            tt = T::FillChecking;
                        }
                    }
                    "posting" | "realTimePosting" | "registerRecordsDeletion"
                    | "registerRecordsWritingOnPost" => {
                        if field.is_none() {
                            cur_posting_prop = Some(cap_first(&local));
                            tt = T::PostingProp;
                        }
                    }
                    "informationRegisterPeriodicity" | "writeMode" | "registerType"
                    | "numberType" | "numberLength" | "numberPeriodicity" | "checkUnique"
                    | "autonumbering" | "hierarchical" | "codeLength" | "descriptionLength" => {
                        if field.is_none() {
                            cur_header_prop = Some(cap_first(&local));
                            tt = T::HeaderProp;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if tt == T::None {
                    buf.clear();
                    continue;
                }
                let txt = t
                    .unescape()
                    .map(|s| s.into_owned())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                match tt {
                    T::FieldName => {
                        if let Some(f) = field.as_mut() {
                            if !txt.is_empty() {
                                f.name = Some(txt);
                            }
                        }
                    }
                    T::TabName => {
                        if !txt.is_empty() {
                            if let Some(i) = cur_tab {
                                out.tabular_sections[i].name = txt;
                            }
                            expecting_tab_name = false;
                        }
                    }
                    T::TypeValue => {
                        if let Some(f) = field.as_mut() {
                            if !txt.is_empty() {
                                f.types.push(edt_type_to_cfg(&txt));
                            }
                        }
                    }
                    T::SynValue => {
                        if let Some(f) = field.as_mut() {
                            if !txt.is_empty() && f.synonym.is_none() {
                                f.synonym = Some(txt);
                            }
                        }
                    }
                    T::FillChecking => {
                        if let Some(f) = field.as_mut() {
                            f.required = txt == "ShowError";
                        }
                    }
                    T::PostingProp => {
                        if let Some(p) = cur_posting_prop.take() {
                            if !txt.is_empty() {
                                out.posting.push((p, txt));
                            }
                        }
                    }
                    T::HeaderProp => {
                        if let Some(p) = cur_header_prop.take() {
                            if !txt.is_empty() {
                                out.properties.push((p, txt));
                            }
                        }
                    }
                    T::None => {}
                }
                tt = T::None;
            }
            Ok(Event::End(e)) => {
                let raw = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let local = local_name(&raw).to_string();
                if local == "standardAttributes" {
                    in_std = false;
                    buf.clear();
                    continue;
                }
                if in_std {
                    buf.clear();
                    continue;
                }
                match local.as_str() {
                    "attributes" | "dimensions" | "resources" | "enumValues" | "commands" => {
                        if let Some(fb) = field.take() {
                            if let Some(name) = fb.name.filter(|n| !n.is_empty()) {
                                match fb.kind {
                                    FieldKind::EnumValue => {
                                        if let Some(s) = fb.synonym {
                                            if !s.is_empty() && s != name {
                                                out.enum_synonyms.push((name.clone(), s));
                                            }
                                        }
                                        out.enum_values.push(name);
                                    }
                                    FieldKind::Command => {
                                        let syn = fb.synonym.filter(|s| !s.is_empty() && s != &name);
                                        out.commands.push((name, syn));
                                    }
                                    _ => {
                                        let f = StructField {
                                            name,
                                            type_str: pretty_types(&fb.types),
                                            synonym: fb.synonym,
                                            required: fb.required,
                                        };
                                        match fb.kind {
                                            FieldKind::Dimension => out.dimensions.push(f),
                                            FieldKind::Resource => out.resources.push(f),
                                            FieldKind::TabAttr => match cur_tab {
                                                Some(i) => out.tabular_sections[i].attributes.push(f),
                                                None => out.attributes.push(f),
                                            },
                                            _ => out.attributes.push(f),
                                        }
                                    }
                                }
                            }
                        }
                        in_type = false;
                        in_synonym = false;
                    }
                    "tabularSections" => {
                        in_tabular = false;
                        cur_tab = None;
                        expecting_tab_name = false;
                    }
                    "synonym" => in_synonym = false,
                    "type" => in_type = false,
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("mdo structure: {}", e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Распарсить `.mdo` объекта в рёбра графа связей данных (`data_links`):
/// ссылочные реквизиты/измерения → рёбра, плюс движения документа
/// (`<registerRecords>` → `recorder`).
pub fn parse_mdo_datalinks_xml(content: &str) -> Result<Vec<DataLinkEdge>> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);
    let mut edges: Vec<DataLinkEdge> = Vec::new();
    let mut buf = Vec::new();

    #[derive(PartialEq)]
    enum T {
        None,
        FieldName,
        TabName,
        TypeValue,
        RegisterRec,
    }

    let mut in_std = false;
    let mut in_tabular = false;
    let mut cur_tab_name: Option<String> = None;
    let mut expecting_tab_name = false;
    let mut field: Option<FieldBuild> = None;
    let mut in_type = false;
    let mut tt = T::None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let raw = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let local = local_name(&raw).to_string();
                if local == "standardAttributes" {
                    in_std = true;
                    buf.clear();
                    continue;
                }
                if in_std {
                    buf.clear();
                    continue;
                }
                match local.as_str() {
                    "tabularSections" => {
                        in_tabular = true;
                        expecting_tab_name = true;
                        cur_tab_name = None;
                    }
                    "attributes" => {
                        field = Some(FieldBuild::new(if in_tabular {
                            FieldKind::TabAttr
                        } else {
                            FieldKind::Attribute
                        }));
                    }
                    "dimensions" => field = Some(FieldBuild::new(FieldKind::Dimension)),
                    "name" => {
                        if let Some(f) = field.as_ref() {
                            if f.name.is_none() {
                                tt = T::FieldName;
                            }
                        } else if expecting_tab_name {
                            tt = T::TabName;
                        }
                    }
                    "type" => {
                        if field.is_some() {
                            in_type = true;
                        }
                    }
                    "types" => {
                        if field.is_some() && in_type {
                            tt = T::TypeValue;
                        }
                    }
                    "registerRecords" => {
                        if field.is_none() {
                            tt = T::RegisterRec;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if tt == T::None {
                    buf.clear();
                    continue;
                }
                let txt = t
                    .unescape()
                    .map(|s| s.into_owned())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                match tt {
                    T::FieldName => {
                        if let Some(f) = field.as_mut() {
                            if !txt.is_empty() {
                                f.name = Some(txt);
                            }
                        }
                    }
                    T::TabName => {
                        if !txt.is_empty() {
                            cur_tab_name = Some(txt);
                            expecting_tab_name = false;
                        }
                    }
                    T::TypeValue => {
                        if let Some(f) = field.as_mut() {
                            if !txt.is_empty() {
                                f.types.push(edt_type_to_cfg(&txt));
                            }
                        }
                    }
                    // Движение документа: `<registerRecords>AccumulationRegister.X`.
                    // Значение уже каноническое имя регистра → прямое ребро recorder.
                    T::RegisterRec => {
                        if !txt.is_empty() {
                            edges.push(DataLinkEdge {
                                from_path: String::new(),
                                to_object: txt,
                                link_kind: "recorder",
                                is_composite: false,
                                is_universal: false,
                            });
                        }
                    }
                    T::None => {}
                }
                tt = T::None;
            }
            Ok(Event::End(e)) => {
                let raw = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let local = local_name(&raw).to_string();
                if local == "standardAttributes" {
                    in_std = false;
                    buf.clear();
                    continue;
                }
                if in_std {
                    buf.clear();
                    continue;
                }
                match local.as_str() {
                    "attributes" | "dimensions" => {
                        if let Some(fb) = field.take() {
                            if let Some(name) = fb.name.filter(|n| !n.is_empty()) {
                                // Классифицируем каждый ссылочный тип в цель ребра.
                                let refs: Vec<(String, bool)> =
                                    fb.types.iter().filter_map(|t| classify_type(t)).collect();
                                let is_composite = refs.len() > 1;
                                let (from_path, link_kind): (String, &'static str) = match fb.kind {
                                    FieldKind::TabAttr => (
                                        match &cur_tab_name {
                                            Some(tn) => format!("{}.{}", tn, name),
                                            None => name.clone(),
                                        },
                                        "tabular_attr",
                                    ),
                                    FieldKind::Dimension => (name.clone(), "register_dim"),
                                    _ => (name.clone(), "attr"),
                                };
                                for (to_object, is_universal) in refs {
                                    edges.push(DataLinkEdge {
                                        from_path: from_path.clone(),
                                        to_object,
                                        link_kind,
                                        is_composite,
                                        is_universal,
                                    });
                                }
                            }
                        }
                        in_type = false;
                    }
                    "tabularSections" => {
                        in_tabular = false;
                        cur_tab_name = None;
                        expecting_tab_name = false;
                    }
                    "type" => in_type = false,
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("mdo data_links: {}", e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(edges)
}

/// Лёгкий парс шапки `.mdo`: meta_type (корневой тег `mdclass:<Тип>`), имя
/// объекта (`<name>` — прямой ребёнок корня) и синоним (`<synonym><value>` ru).
/// Возвращает `None`, если корневой тег/имя не распознаны.
pub fn parse_mdo_header(content: &str) -> Option<(String, String, Option<String>)> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    let mut depth = 0i32;
    let mut meta_type: Option<String> = None;
    let mut name: Option<String> = None;
    let mut synonym: Option<String> = None;

    let mut want_name = false; // ждём текст прямого <name> объекта (depth 2)
    let mut in_obj_synonym = false; // внутри прямого <synonym> объекта
    let mut want_syn_value = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                depth += 1;
                let raw = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let local = local_name(&raw).to_string();
                // Корневой тег mdclass:<Тип> на depth 1 — это meta_type.
                if depth == 1 {
                    meta_type = Some(local.clone());
                } else if depth == 2 {
                    // Состав объекта начался — шапка позади, дальше не нужно.
                    if matches!(
                        local.as_str(),
                        "attributes"
                            | "tabularSections"
                            | "dimensions"
                            | "resources"
                            | "enumValues"
                            | "commands"
                            | "forms"
                    ) {
                        break;
                    }
                    if local == "name" && name.is_none() {
                        want_name = true;
                    } else if local == "synonym" && synonym.is_none() {
                        in_obj_synonym = true;
                    }
                } else if depth == 3 && in_obj_synonym && local == "value" {
                    want_syn_value = true;
                }
            }
            Ok(Event::Text(t)) => {
                if want_name || want_syn_value {
                    let txt = t
                        .unescape()
                        .map(|s| s.into_owned())
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if want_name {
                        if !txt.is_empty() {
                            name = Some(txt);
                        }
                        want_name = false;
                    } else if want_syn_value {
                        if !txt.is_empty() {
                            synonym = Some(txt);
                        }
                        want_syn_value = false;
                    }
                }
            }
            Ok(Event::End(e)) => {
                let raw = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if local_name(&raw) == "synonym" {
                    in_obj_synonym = false;
                }
                depth -= 1;
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    match (meta_type, name) {
        (Some(mt), Some(nm)) => Some((mt, nm, synonym)),
        _ => None,
    }
}

/// Разобрать обработчики событий формы из EDT `Form.form`.
/// Form-level и element-level обработчики записаны единообразно:
/// `<handlers><event>OnCreateAtServer</event><name>ПриСозданииНаСервере</name></handlers>`,
/// где `event` — англ. имя события платформы, `name` — имя процедуры-обработчика
/// в модуле формы. Возвращает пары `(event, handler)`; `event` приводится к
/// русскому имени через `event_to_russian` — чтобы совпадать с форматом
/// Конфигуратора (где события формы по-русски). Привязки команд
/// (`<action ...><handler>`) — это не event-обработчики, их не берём.
pub fn parse_mdo_form_handlers(content: &str) -> Vec<(String, String)> {
    use super::event_subscriptions::event_to_russian;

    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);
    let mut out: Vec<(String, String)> = Vec::new();
    let mut buf = Vec::new();

    let mut in_handlers = false;
    let mut cur_event: Option<String> = None;
    let mut cur_name: Option<String> = None;
    // 0 — нет; 1 — ждём текст <event>; 2 — ждём текст <name>.
    let mut tt = 0u8;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = local_name(&String::from_utf8_lossy(e.name().as_ref())).to_string();
                match local.as_str() {
                    "handlers" => {
                        in_handlers = true;
                        cur_event = None;
                        cur_name = None;
                    }
                    "event" if in_handlers => tt = 1,
                    "name" if in_handlers => tt = 2,
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if tt != 0 {
                    let txt = t
                        .unescape()
                        .map(|s| s.into_owned())
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if tt == 1 && !txt.is_empty() {
                        cur_event = Some(txt);
                    } else if tt == 2 && !txt.is_empty() {
                        cur_name = Some(txt);
                    }
                    tt = 0;
                }
            }
            Ok(Event::End(e)) => {
                let local = local_name(&String::from_utf8_lossy(e.name().as_ref())).to_string();
                if local == "handlers" {
                    if let (Some(ev), Some(nm)) = (cur_event.take(), cur_name.take()) {
                        out.push((event_to_russian(&ev).to_string(), nm));
                    }
                    in_handlers = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Разобрать `.mdo` подписки на событие (EventSubscriptions/<Имя>/<Имя>.mdo).
/// Возвращает `(name, event_ru, handler_module, handler_proc, sources)`:
/// `event` нормализуется к русскому виду (как у формата Конфигуратора),
/// `handler` (`CommonModule.Модуль.Процедура`) режется по последней точке на
/// модуль и процедуру, `sources` — типы источников (`DocumentObject.X`).
/// Возвращает `None`, если нет name/event/handler.
pub fn parse_mdo_event_subscription(
    content: &str,
) -> Option<(String, String, String, String, Vec<String>)> {
    use super::event_subscriptions::event_to_russian;

    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    let mut depth = 0i32;
    let mut name: Option<String> = None;
    let mut event: Option<String> = None;
    let mut handler: Option<String> = None;
    let mut sources: Vec<String> = Vec::new();
    let mut in_source = false;
    // 0 нет; 1 name; 2 event; 3 handler; 4 types(source).
    let mut tt = 0u8;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                depth += 1;
                let local = local_name(&String::from_utf8_lossy(e.name().as_ref())).to_string();
                if depth == 2 {
                    match local.as_str() {
                        "name" if name.is_none() => tt = 1,
                        "event" => tt = 2,
                        "handler" => tt = 3,
                        "source" => in_source = true,
                        _ => {}
                    }
                } else if in_source && local == "types" {
                    tt = 4;
                }
            }
            Ok(Event::Text(t)) => {
                if tt != 0 {
                    let txt = t
                        .unescape()
                        .map(|s| s.into_owned())
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if !txt.is_empty() {
                        match tt {
                            1 => name = Some(txt),
                            2 => event = Some(txt),
                            3 => handler = Some(txt),
                            4 => sources.push(txt),
                            _ => {}
                        }
                    }
                    tt = 0;
                }
            }
            Ok(Event::End(e)) => {
                let local = local_name(&String::from_utf8_lossy(e.name().as_ref())).to_string();
                if local == "source" {
                    in_source = false;
                }
                depth -= 1;
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    let (n, ev, h) = (name?, event?, handler?);
    let (module, proc_) = match h.rfind('.') {
        Some(i) => (h[..i].to_string(), h[i + 1..].to_string()),
        None => (String::new(), h),
    };
    Some((n, event_to_russian(&ev).to_string(), module, proc_, sources))
}

/// Определить, является ли репо проектом 1C:EDT, и вернуть корень с папками
/// типов (`src/`, где лежат `Catalogs/`, `Documents/` и т.д.).
/// Признак EDT: файл `<...>/Configuration/Configuration.mdo`.
pub fn detect_edt_src(repo_root: &Path) -> Option<PathBuf> {
    for entry in WalkDir::new(repo_root)
        .max_depth(4)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file()
            && entry.file_name().to_str() == Some("Configuration.mdo")
        {
            if let Some(cfg_dir) = entry.path().parent() {
                if cfg_dir.file_name().and_then(|s| s.to_str()) == Some("Configuration") {
                    if let Some(src) = cfg_dir.parent() {
                        return Some(src.to_path_buf());
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG_MDO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<mdclass:Catalog xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:core="http://g5.1c.ru/v8/dt/mcore" xmlns:mdclass="http://g5.1c.ru/v8/dt/metadata/mdclass" uuid="cf336ad9">
  <name>АвансовыйОтчетПрисоединенныеФайлы</name>
  <synonym><key>ru</key><value>Присоединенные файлы</value></synonym>
  <standardAttributes>
    <name>Description</name>
    <synonym><key>ru</key><value>Имя файла</value></synonym>
  </standardAttributes>
  <attributes uuid="301fe396">
    <name>Автор</name>
    <synonym><key>ru</key><value>Автор</value></synonym>
    <type>
      <types>CatalogRef.ВнешниеПользователи</types>
      <types>CatalogRef.Пользователи</types>
    </type>
    <toolTip><key>ru</key><value>Пользователь, который добавил файл</value></toolTip>
    <fillChecking>ShowError</fillChecking>
  </attributes>
  <attributes uuid="6f5bef20">
    <name>ДатаСоздания</name>
    <type><types>Date</types></type>
  </attributes>
</mdclass:Catalog>"#;

    #[test]
    fn header_extracts_meta_type_name_synonym() {
        let h = parse_mdo_header(CATALOG_MDO).unwrap();
        assert_eq!(h.0, "Catalog");
        assert_eq!(h.1, "АвансовыйОтчетПрисоединенныеФайлы");
        assert_eq!(h.2.as_deref(), Some("Присоединенные файлы"));
    }

    #[test]
    fn structure_skips_standard_attrs_and_reads_user_attrs() {
        let s = parse_mdo_structure_xml(CATALOG_MDO).unwrap();
        assert_eq!(s.attributes.len(), 2);
        assert_eq!(s.attributes[0].name, "Автор");
        // Составной ссылочный тип → русская нотация через " | ".
        assert_eq!(
            s.attributes[0].type_str,
            "СправочникСсылка.ВнешниеПользователи | СправочникСсылка.Пользователи"
        );
        assert_eq!(s.attributes[0].synonym.as_deref(), Some("Автор"));
        assert!(s.attributes[0].required);
        assert_eq!(s.attributes[1].name, "ДатаСоздания");
        assert_eq!(s.attributes[1].type_str, "Дата");
        // standardAttributes (Description) НЕ попадает в пользовательские реквизиты.
        assert!(!s.attributes.iter().any(|f| f.name == "Description"));
    }

    #[test]
    fn datalinks_emit_referential_edges_composite() {
        let edges = parse_mdo_datalinks_xml(CATALOG_MDO).unwrap();
        // Автор → 2 ссылочных ребра (составной), ДатаСоздания (Date) — не ссылка.
        assert_eq!(edges.len(), 2);
        assert!(edges
            .iter()
            .all(|e| e.from_path == "Автор" && e.link_kind == "attr" && e.is_composite));
        assert!(edges.iter().any(|e| e.to_object == "Catalog.ВнешниеПользователи"));
        assert!(edges.iter().any(|e| e.to_object == "Catalog.Пользователи"));
    }

    const DOC_MDO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<mdclass:Document xmlns:mdclass="http://g5.1c.ru/v8/dt/metadata/mdclass" uuid="abc">
  <name>АвансовыйОтчет</name>
  <synonym><key>ru</key><value>Авансовый отчёт</value></synonym>
  <posting>Allow</posting>
  <registerRecordsDeletion>AutoDeleteOff</registerRecordsDeletion>
  <registerRecords>AccumulationRegister.ПрочиеРасчеты</registerRecords>
  <registerRecords>AccountingRegister.Хозрасчетный</registerRecords>
  <attributes uuid="1"><name>Организация</name><type><types>CatalogRef.Организации</types></type></attributes>
  <tabularSections uuid="2">
    <name>Товары</name>
    <attributes uuid="3"><name>Номенклатура</name><type><types>CatalogRef.Номенклатура</types></type></attributes>
  </tabularSections>
</mdclass:Document>"#;

    #[test]
    fn document_posting_recorder_and_tabular() {
        let s = parse_mdo_structure_xml(DOC_MDO).unwrap();
        assert_eq!(s.attributes.len(), 1);
        assert_eq!(s.attributes[0].name, "Организация");
        assert_eq!(s.tabular_sections.len(), 1);
        assert_eq!(s.tabular_sections[0].name, "Товары");
        assert_eq!(s.tabular_sections[0].attributes[0].name, "Номенклатура");
        // Свойства проведения.
        assert!(s.posting.iter().any(|(k, v)| k == "Posting" && v == "Allow"));
        assert!(s
            .posting
            .iter()
            .any(|(k, v)| k == "RegisterRecordsDeletion" && v == "AutoDeleteOff"));

        let edges = parse_mdo_datalinks_xml(DOC_MDO).unwrap();
        // recorder: 2 регистра; attr: Организация; tabular_attr: Товары.Номенклатура.
        let rec: Vec<_> = edges.iter().filter(|e| e.link_kind == "recorder").collect();
        assert_eq!(rec.len(), 2);
        assert!(rec.iter().any(|e| e.to_object == "AccumulationRegister.ПрочиеРасчеты"));
        assert!(edges
            .iter()
            .any(|e| e.link_kind == "attr" && e.to_object == "Catalog.Организации"));
        assert!(edges.iter().any(|e| e.link_kind == "tabular_attr"
            && e.from_path == "Товары.Номенклатура"
            && e.to_object == "Catalog.Номенклатура"));
    }

    const REGISTER_MDO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<mdclass:AccumulationRegister xmlns:mdclass="http://g5.1c.ru/v8/dt/metadata/mdclass" uuid="r">
  <name>ТоварыНаСкладах</name>
  <registerType>Balance</registerType>
  <resources uuid="r1"><name>Количество</name><type><types>Number</types></type></resources>
  <dimensions uuid="d1"><name>Склад</name><type><types>CatalogRef.Склады</types></type></dimensions>
  <dimensions uuid="d2"><name>Номенклатура</name><type><types>CatalogRef.Номенклатура</types></type></dimensions>
</mdclass:AccumulationRegister>"#;

    #[test]
    fn register_dimensions_resources_and_dim_links() {
        let s = parse_mdo_structure_xml(REGISTER_MDO).unwrap();
        assert_eq!(s.dimensions.len(), 2);
        assert_eq!(s.resources.len(), 1);
        assert_eq!(s.resources[0].type_str, "Число");
        assert!(s.properties.iter().any(|(k, v)| k == "RegisterType" && v == "Balance"));

        let edges = parse_mdo_datalinks_xml(REGISTER_MDO).unwrap();
        // Оба измерения ссылочные → register_dim; ресурс (Число) — не ссылка.
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|e| e.link_kind == "register_dim"));
        assert!(edges.iter().any(|e| e.to_object == "Catalog.Склады"));
    }

    const ENUM_MDO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<mdclass:Enum xmlns:mdclass="http://g5.1c.ru/v8/dt/metadata/mdclass" uuid="e">
  <name>ВидыЦен</name>
  <enumValues uuid="e1"><name>Оптовая</name><synonym><key>ru</key><value>Оптовая цена</value></synonym></enumValues>
  <enumValues uuid="e2"><name>Розничная</name></enumValues>
</mdclass:Enum>"#;

    #[test]
    fn enum_values_and_synonyms() {
        let s = parse_mdo_structure_xml(ENUM_MDO).unwrap();
        assert_eq!(s.enum_values, vec!["Оптовая", "Розничная"]);
        assert_eq!(
            s.enum_synonyms,
            vec![("Оптовая".to_string(), "Оптовая цена".to_string())]
        );
    }

    #[test]
    fn edt_type_normalization() {
        assert_eq!(edt_type_to_cfg("String"), "xs:string");
        assert_eq!(edt_type_to_cfg("Number"), "xs:decimal");
        assert_eq!(edt_type_to_cfg("CatalogRef.Товары"), "cfg:CatalogRef.Товары");
        assert_eq!(edt_type_to_cfg("DefinedType.Сумма"), "cfg:DefinedType.Сумма");
        assert_eq!(edt_type_to_cfg("AnyRef"), "cfg:AnyRef");
    }

    #[test]
    fn form_handlers_extracted_actions_skipped() {
        let form = r#"<?xml version="1.0" encoding="UTF-8"?>
<form:Form xmlns:form="http://g5.1c.ru/v8/dt/form" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <handlers><event>OnCreateAtServer</event><name>ПриСозданииНаСервере</name></handlers>
  <handlers><event>Selection</event><name>СписокВыбор</name></handlers>
  <action xsi:type="form:FormCommandHandlerContainer"><handler>НеБрать</handler></action>
</form:Form>"#;
        let h = parse_mdo_form_handlers(form);
        assert_eq!(h.len(), 2);
        assert!(h.iter().any(|(_, n)| n == "ПриСозданииНаСервере"));
        assert!(h.iter().any(|(_, n)| n == "СписокВыбор"));
        // Привязка команды (<action><handler>) — не event-обработчик, не берём.
        assert!(!h.iter().any(|(_, n)| n == "НеБрать"));
    }

    #[test]
    fn event_subscription_parsed() {
        let mdo = r#"<?xml version="1.0" encoding="UTF-8"?>
<mdclass:EventSubscription xmlns:mdclass="http://g5.1c.ru/v8/dt/metadata/mdclass" uuid="x">
  <name>ПодписьПриПроведении</name>
  <synonym><key>ru</key><value>Подпись при проведении</value></synonym>
  <source><types>DocumentObject.АвансовыйОтчет</types></source>
  <event>Posting</event>
  <handler>CommonModule.УчетЗарплаты.ПриПроведении</handler>
</mdclass:EventSubscription>"#;
        let (name, event, module, proc_, sources) = parse_mdo_event_subscription(mdo).unwrap();
        assert_eq!(name, "ПодписьПриПроведении");
        // Posting → русское событие (как у формата Конфигуратора).
        assert_eq!(event, "ОбработкаПроведения");
        assert_eq!(module, "CommonModule.УчетЗарплаты");
        assert_eq!(proc_, "ПриПроведении");
        assert_eq!(sources, vec!["DocumentObject.АвансовыйОтчет"]);
    }
}
