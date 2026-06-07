// Парсер ссылочных типов реквизитов/измерений из XML отдельного объекта 1С.
//
// Источник — файлы вида `Catalogs/<X>.xml`, `Documents/<Y>.xml`,
// `AccumulationRegisters/<Z>.xml` и т.д. (выгрузка DumpConfigToFiles).
// Из каждого реквизита шапки, реквизита табличной части и измерения регистра
// извлекаются ссылочные типы и превращаются в рёбра графа связей данных
// (`data_links`): `<owner> --[from_path]--> <target>`.
//
// Реальная структура XML объекта (фрагмент Catalog из УТ):
//
//   <MetaDataObject>
//     <Catalog uuid="...">
//       <Properties><Name>КлючиАналитики...</Name>...</Properties>
//       <ChildObjects>
//         <Attribute uuid="...">
//           <Properties>
//             <Name>Контрагент</Name>
//             ...
//             <Type>
//               <v8:Type>cfg:CatalogRef.Организации</v8:Type>
//               <v8:Type>cfg:CatalogRef.Контрагенты</v8:Type>   ← составной
//             </Type>
//           </Properties>
//         </Attribute>
//         <TabularSection uuid="...">
//           <Properties><Name>Товары</Name></Properties>
//           <ChildObjects>
//             <Attribute><Properties><Name>Номенклатура</Name>
//               <Type><v8:Type>cfg:CatalogRef.Номенклатура</v8:Type></Type>
//             </Properties></Attribute>
//           </ChildObjects>
//         </TabularSection>
//       </ChildObjects>
//     </Catalog>
//   </MetaDataObject>
//
// Регистры: вместо <Attribute> — <Dimension> (измерения) и <Resource>.
// Измерения почти всегда ссылочные → link_kind = "register_dim".
//
// Классификация типа (см. `classify_type`):
//   * `cfg:CatalogRef.Контрагенты`        → ребро в `Catalog.Контрагенты` (конкретное);
//   * несколько `<v8:Type>` подряд        → несколько рёбер, is_composite=1;
//   * `cfg:CatalogRef` (имени нет)         → `*CatalogRef`, is_universal=1 (терминал);
//   * `cfg:AnyRef`                         → `*AnyRef`, is_universal=1;
//   * `cfg:DefinedType.Организация`        → `*DefinedType.Организация`, is_universal=1
//     (резолв определяемых типов в конкретные — этап 2);
//   * `xs:string` / `xs:decimal` / `v8:*`  → не ссылка, пропуск.

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde_json::{json, Value};
use std::path::Path;

/// Страховочный предел на число конкретных типов в составном реквизите.
/// Перечни в реальных конфигурациях короткие (2–20); если перечислено
/// больше — это патология, схлопываем в один терминальный `*Multiple`-узел,
/// чтобы не плодить десятки рёбер от одного поля.
const MAX_COMPOSITE_TARGETS: usize = 30;

/// Одно ребро графа связей данных, исходящее из объекта-владельца.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataLinkEdge {
    /// Путь к реквизиту: `Контрагент` либо `Товары.Номенклатура` (ТЧ.реквизит),
    /// для измерения регистра — имя измерения.
    pub from_path: String,
    /// Цель: `Catalog.Контрагенты` (конкретная) либо `*CatalogRef` / `*AnyRef`
    /// / `*DefinedType.X` (обобщённая, терминал обхода).
    pub to_object: String,
    /// Тип ребра: `attr` | `tabular_attr` | `register_dim` | `recorder`.
    /// `recorder` — движение документа в регистр (документ → регистр),
    /// источник — `<RegisterRecords>` в XML документа. У него `from_path`
    /// пуст (это не реквизит), `to_object` — полное имя регистра.
    pub link_kind: &'static str,
    /// Ребро из составного типа (перечислено несколько конкретных типов).
    pub is_composite: bool,
    /// Обобщённый тип, схлопнут в `*`-узел.
    pub is_universal: bool,
}

/// Прочитать и распарсить файл объекта по пути.
/// `owner_full_name` — канонический идентификатор владельца (`Catalog.X`).
/// Возвращает `Ok(Vec::new())`, если файла нет.
pub fn parse_object_attributes_file(
    path: &Path,
    _owner_full_name: &str,
) -> Result<Vec<DataLinkEdge>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Не удалось прочитать {}", path.display()))?;
    parse_object_attributes_xml(&content)
}

/// Накопитель состояния текущего разбираемого поля (реквизит/измерение).
struct FieldAccum {
    name: Option<String>,
    kind: &'static str,
    types: Vec<String>,
}

/// Куда направить ближайший текстовый узел.
#[derive(PartialEq)]
enum TextTarget {
    None,
    FieldName,
    TabularName,
    TypeValue,
    /// Текст `<xr:Item>` внутри `<RegisterRecords>` — имя регистра-приёмника.
    RegisterRef,
}

/// Распарсить содержимое XML объекта в список рёбер связей данных.
/// `owner_full_name` не нужен парсеру (рёбра возвращаются без владельца —
/// его проставляет вызывающий при вставке), но имя поля/ТЧ берётся из XML.
pub fn parse_object_attributes_xml(content: &str) -> Result<Vec<DataLinkEdge>> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut out: Vec<DataLinkEdge> = Vec::new();
    let mut buf = Vec::new();

    // Имя текущей табличной части (Some, пока мы внутри <TabularSection>).
    let mut tabular: Option<String> = None;
    // Ждём <Name> табличной части (вошли в TabularSection, имя ещё не взяли).
    let mut expecting_tabular_name = false;
    // Текущее разбираемое поле (Attribute/Dimension/Resource).
    let mut field: Option<FieldAccum> = None;
    // Внутри контейнера <Type> (не <v8:Type>).
    let mut in_type = false;
    // Внутри <RegisterRecords> — список регистров, в которые пишет документ.
    let mut in_register_records = false;
    let mut text_target = TextTarget::None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let raw = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let local = local_name(&raw);
                match local.as_str() {
                    "TabularSection" => {
                        // Имя ТЧ придёт в её Properties/Name.
                        expecting_tabular_name = true;
                    }
                    "Attribute" | "Dimension" | "Resource" => {
                        let kind = if local == "Dimension" {
                            "register_dim"
                        } else if tabular.is_some() {
                            "tabular_attr"
                        } else {
                            "attr"
                        };
                        field = Some(FieldAccum { name: None, kind, types: Vec::new() });
                    }
                    "Name" => {
                        // Имя поля: внутри текущего field, ещё не взято.
                        if let Some(f) = field.as_ref() {
                            if f.name.is_none() {
                                text_target = TextTarget::FieldName;
                            }
                        } else if expecting_tabular_name {
                            text_target = TextTarget::TabularName;
                        }
                    }
                    "RegisterRecords" => {
                        // Состав движений документа: <xr:Item> внутри —
                        // полные имена регистров, в которые документ пишет.
                        in_register_records = true;
                    }
                    "Item" if in_register_records => {
                        // Текст <xr:Item> — каноническое имя регистра-приёмника.
                        text_target = TextTarget::RegisterRef;
                    }
                    _ => {
                        // Различаем контейнер <Type> и элемент <v8:Type>.
                        // local_name у обоих == "Type" — смотрим сырое имя.
                        if raw == "Type" {
                            if field.is_some() {
                                in_type = true;
                            }
                        } else if raw.ends_with(":Type") {
                            // <v8:Type> — собрать его текст как значение типа.
                            if field.is_some() && in_type {
                                text_target = TextTarget::TypeValue;
                            }
                        }
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if text_target == TextTarget::None {
                    buf.clear();
                    continue;
                }
                let txt = t.unescape().map(|s| s.into_owned()).unwrap_or_default();
                let txt = txt.trim().to_string();
                match text_target {
                    TextTarget::FieldName => {
                        if let Some(f) = field.as_mut() {
                            if !txt.is_empty() {
                                f.name = Some(txt);
                            }
                        }
                    }
                    TextTarget::TabularName => {
                        if !txt.is_empty() {
                            tabular = Some(txt);
                            expecting_tabular_name = false;
                        }
                    }
                    TextTarget::TypeValue => {
                        if let Some(f) = field.as_mut() {
                            if !txt.is_empty() {
                                f.types.push(txt);
                            }
                        }
                    }
                    TextTarget::RegisterRef => {
                        // Документ → регистр: ребро recorder. Цель уже
                        // в каноническом виде (AccumulationRegister.X и т.п.).
                        if !txt.is_empty() {
                            out.push(DataLinkEdge {
                                from_path: String::new(),
                                to_object: txt,
                                link_kind: "recorder",
                                is_composite: false,
                                is_universal: false,
                            });
                        }
                    }
                    TextTarget::None => {}
                }
                text_target = TextTarget::None;
            }
            Ok(Event::End(e)) => {
                let raw = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let local = local_name(&raw);
                match local.as_str() {
                    "Attribute" | "Dimension" | "Resource" => {
                        if let Some(f) = field.take() {
                            emit_field_edges(&f, tabular.as_deref(), &mut out);
                        }
                        in_type = false;
                    }
                    "TabularSection" => {
                        tabular = None;
                    }
                    "RegisterRecords" => {
                        in_register_records = false;
                    }
                    _ => {
                        if raw == "Type" {
                            in_type = false;
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "object XML: ошибка парсинга на позиции {}: {}",
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

/// Сформировать рёбра из накопленного поля и дописать в `out`.
fn emit_field_edges(f: &FieldAccum, tabular: Option<&str>, out: &mut Vec<DataLinkEdge>) {
    let name = match f.name.as_ref() {
        Some(n) if !n.is_empty() => n,
        _ => return,
    };
    // Классифицируем все типы поля; оставляем только ссылочные.
    let mut targets: Vec<(String, bool)> = f
        .types
        .iter()
        .filter_map(|t| classify_type(t))
        .collect();
    if targets.is_empty() {
        return;
    }
    // Дедуп (составной тип может повторять одну цель).
    targets.sort();
    targets.dedup();

    let from_path = match tabular {
        Some(ts) => format!("{}.{}", ts, name),
        None => name.clone(),
    };

    // Страховочный cap: патологический перечень → один терминальный узел.
    if targets.len() > MAX_COMPOSITE_TARGETS {
        out.push(DataLinkEdge {
            from_path,
            to_object: "*Multiple".to_string(),
            link_kind: f.kind,
            is_composite: true,
            is_universal: true,
        });
        return;
    }

    let is_composite = targets.len() > 1;
    for (to_object, is_universal) in targets {
        out.push(DataLinkEdge {
            from_path: from_path.clone(),
            to_object,
            link_kind: f.kind,
            is_composite,
            is_universal,
        });
    }
}

/// Классифицировать строку типа из `<v8:Type>`.
/// Возвращает `Some((to_object, is_universal))` для ссылочных типов,
/// `None` для примитивов и платформенных типов (не рёбра графа данных).
pub fn classify_type(s: &str) -> Option<(String, bool)> {
    let s = s.trim();
    // Ссылки на объекты конфигурации идут с префиксом `cfg:`.
    let rest = s.strip_prefix("cfg:")?;

    // Любая ссылка.
    if rest == "AnyRef" {
        return Some(("*AnyRef".to_string(), true));
    }
    // Определяемый тип — резолв в конкретику на этапе 2, пока терминал.
    if let Some(dt) = rest.strip_prefix("DefinedType.") {
        if dt.is_empty() {
            return None;
        }
        return Some((format!("*DefinedType.{}", dt), true));
    }

    match rest.split_once('.') {
        // Конкретный тип: `<Kind>Ref.<Name>` → `<Kind>.<Name>`.
        Some((kind_ref, name)) => {
            let kind = kind_ref.strip_suffix("Ref")?;
            if kind.is_empty() || name.is_empty() {
                return None;
            }
            Some((format!("{}.{}", kind, name), false))
        }
        // Обобщённый тип «вся категория»: `cfg:CatalogRef` без имени.
        None => {
            let kind = rest.strip_suffix("Ref")?;
            if kind.is_empty() || kind == "Any" {
                Some(("*AnyRef".to_string(), true))
            } else {
                Some((format!("*{}Ref", kind), true))
            }
        }
    }
}

/// Имя тега без namespace-префикса (`v8:Type` → `Type`).
fn local_name(name: &str) -> String {
    match name.find(':') {
        Some(idx) => name[idx + 1..].to_string(),
        None => name.to_string(),
    }
}

// ── Полная структура объекта (для get_object_structure) ────────────────────
//
// В отличие от парсера рёбер выше (он оставляет только ссылочные типы),
// здесь собираем ВСЕ реквизиты с их типами (включая примитивы Строка/Число/
// Дата), табличные части с их реквизитами, а также измерения и ресурсы
// регистров. Результат сериализуется в `metadata_objects.attributes_json`
// и отдаётся MCP-tool `get_object_structure`.

/// Реквизит/измерение/ресурс: имя + человекочитаемый тип.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructField {
    pub name: String,
    /// Тип в 1С-нотации: `Строка`, `Число`, `СправочникСсылка.Номенклатура`,
    /// составной — через ` | `. Пустой тип → `—`.
    pub type_str: String,
}

/// Табличная часть: имя + её реквизиты.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructTabular {
    pub name: String,
    pub attributes: Vec<StructField>,
}

/// Полная структура объекта конфигурации.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectStructure {
    pub attributes: Vec<StructField>,
    pub dimensions: Vec<StructField>,
    pub resources: Vec<StructField>,
    pub tabular_sections: Vec<StructTabular>,
    /// Значения перечисления (только для meta_type = Enum), порядок из XML.
    pub enum_values: Vec<String>,
    /// Имена предопределённых элементов (Catalog/ChartOfAccounts/ChartOf*),
    /// из соседнего `<Объект>/Ext/Predefined.xml`. Порядок из XML.
    pub predefined: Vec<String>,
}

impl ObjectStructure {
    /// Пусто ли (нет ни одного поля) — такие объекты не пишем в индекс.
    pub fn is_empty(&self) -> bool {
        self.attributes.is_empty()
            && self.dimensions.is_empty()
            && self.resources.is_empty()
            && self.tabular_sections.is_empty()
            && self.enum_values.is_empty()
            && self.predefined.is_empty()
    }

    /// Сериализовать в JSON для `attributes_json` (пустые секции опускаем).
    pub fn to_json(&self) -> Value {
        let field = |f: &StructField| json!({ "name": f.name, "type": f.type_str });
        // B1: базовые секции эмитятся ВСЕГДА (пустые → []), чтобы агент
        // отличал «секции нет» от «инструмент её не отдаёт» и не уходил в XML.
        let ts: Vec<Value> = self
            .tabular_sections
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "attributes": t.attributes.iter().map(field).collect::<Vec<_>>(),
                })
            })
            .collect();
        let mut map = serde_json::Map::new();
        map.insert(
            "attributes".into(),
            Value::Array(self.attributes.iter().map(field).collect()),
        );
        map.insert(
            "dimensions".into(),
            Value::Array(self.dimensions.iter().map(field).collect()),
        );
        map.insert(
            "resources".into(),
            Value::Array(self.resources.iter().map(field).collect()),
        );
        map.insert("tabular_sections".into(), Value::Array(ts));
        // B2: enum_values — только для перечислений (у прочих объектов пусто).
        if !self.enum_values.is_empty() {
            map.insert(
                "enum_values".into(),
                Value::Array(
                    self.enum_values
                        .iter()
                        .map(|v| Value::String(v.clone()))
                        .collect(),
                ),
            );
        }
        // C2: predefined — имена предопределённых элементов (если есть).
        if !self.predefined.is_empty() {
            map.insert(
                "predefined".into(),
                Value::Array(
                    self.predefined
                        .iter()
                        .map(|v| Value::String(v.clone()))
                        .collect(),
                ),
            );
        }
        Value::Object(map)
    }

    /// Слить структуру из другой sub-config (расширения) в эту (обычно base).
    /// Union по имени: поля/ТЧ/значения из `other`, которых ещё нет в `self`,
    /// добавляются в конец; одноимённые сохраняют версию `self` (base-приоритет
    /// типа). Для одноимённых табличных частей объединяются их реквизиты.
    ///
    /// Нужно потому, что объект в расширениях ДОБАВЛЯЕТ реквизиты к базовому, а
    /// `attributes_json` — единый блоб на объект: без мерджа последняя
    /// обработанная sub-config затирала бы базовую структуру (баг до 0.21.0 —
    /// тяжёлый документ с 145 реквизитами получал 1 реквизит из расширения).
    pub fn merge_from(&mut self, other: &ObjectStructure) {
        merge_fields(&mut self.attributes, &other.attributes);
        merge_fields(&mut self.dimensions, &other.dimensions);
        merge_fields(&mut self.resources, &other.resources);
        for ot in &other.tabular_sections {
            match self.tabular_sections.iter_mut().find(|t| t.name == ot.name) {
                Some(existing) => merge_fields(&mut existing.attributes, &ot.attributes),
                None => self.tabular_sections.push(ot.clone()),
            }
        }
        merge_names(&mut self.enum_values, &other.enum_values);
        merge_names(&mut self.predefined, &other.predefined);
    }
}

/// Добавить поля из `add`, которых ещё нет в `into` (сравнение по имени).
/// Существующие одноимённые сохраняют версию `into` (base-приоритет).
fn merge_fields(into: &mut Vec<StructField>, add: &[StructField]) {
    for f in add {
        if !into.iter().any(|e| e.name == f.name) {
            into.push(f.clone());
        }
    }
}

/// Добавить строки из `add`, которых ещё нет в `into` (порядок сохраняется).
fn merge_names(into: &mut Vec<String>, add: &[String]) {
    for n in add {
        if !into.iter().any(|e| e == n) {
            into.push(n.clone());
        }
    }
}

/// Прочитать и распарсить полную структуру объекта по пути.
/// `Ok(None)` — если файла нет.
pub fn parse_object_structure_file(path: &Path) -> Result<Option<ObjectStructure>> {
    if !path.is_file() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Не удалось прочитать {}", path.display()))?;
    let mut structure = parse_object_structure_xml(&content)?;

    // C2: предопределённые элементы — в соседнем `<Объект>/Ext/Predefined.xml`
    // (Catalog/ChartOfAccounts/ChartOf*). path `<...>/Catalogs/Качество.xml`
    // → `<...>/Catalogs/Качество/Ext/Predefined.xml`.
    let predef = path.with_extension("").join("Ext").join("Predefined.xml");
    if predef.is_file() {
        if let Ok(pc) = std::fs::read_to_string(&predef) {
            structure.predefined = parse_predefined_xml(&pc);
        }
    }

    Ok(Some(structure))
}

/// Распарсить `Predefined.xml` объекта в список имён предопределённых
/// элементов — `<Item>/<Name>` (первое имя в каждом `<Item>`).
pub fn parse_predefined_xml(content: &str) -> Vec<String> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);
    let mut out: Vec<String> = Vec::new();
    let mut buf = Vec::new();
    // Внутри <Item> и имя ещё не взято.
    let mut in_item = false;
    let mut want_name = false;
    let mut take_text = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = local_name(&String::from_utf8_lossy(e.name().as_ref()));
                if local == "Item" {
                    in_item = true;
                    want_name = true;
                } else if local == "Name" && in_item && want_name {
                    take_text = true;
                }
            }
            Ok(Event::Text(t)) => {
                if take_text {
                    let txt = t.unescape().map(|s| s.into_owned()).unwrap_or_default();
                    let txt = txt.trim().to_string();
                    if !txt.is_empty() {
                        out.push(txt);
                        want_name = false;
                    }
                    take_text = false;
                }
            }
            Ok(Event::End(e)) => {
                let local = local_name(&String::from_utf8_lossy(e.name().as_ref()));
                if local == "Item" {
                    in_item = false;
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

/// Распарсить содержимое XML объекта в полную структуру.
pub fn parse_object_structure_xml(content: &str) -> Result<ObjectStructure> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut out = ObjectStructure::default();
    let mut buf = Vec::new();

    // Индекс текущей табличной части (Some, пока мы внутри <TabularSection>).
    let mut cur_tab: Option<usize> = None;
    let mut expecting_tabular_name = false;
    // Текущее разбираемое поле: (kind, name, types).
    let mut field: Option<(String, Option<String>, Vec<String>)> = None;
    let mut in_type = false;
    let mut text_target = TextTarget::None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let raw = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let local = local_name(&raw);
                match local.as_str() {
                    "TabularSection" => {
                        expecting_tabular_name = true;
                    }
                    "Attribute" | "Dimension" | "Resource" | "EnumValue" => {
                        field = Some((local, None, Vec::new()));
                    }
                    "Name" => {
                        if let Some((_, name, _)) = field.as_ref() {
                            if name.is_none() {
                                text_target = TextTarget::FieldName;
                            }
                        } else if expecting_tabular_name {
                            text_target = TextTarget::TabularName;
                        }
                    }
                    _ => {
                        if raw == "Type" {
                            if field.is_some() {
                                in_type = true;
                            }
                        } else if raw.ends_with(":Type") && field.is_some() && in_type {
                            text_target = TextTarget::TypeValue;
                        }
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if text_target == TextTarget::None {
                    buf.clear();
                    continue;
                }
                let txt = t.unescape().map(|s| s.into_owned()).unwrap_or_default();
                let txt = txt.trim().to_string();
                match text_target {
                    TextTarget::FieldName => {
                        if let Some((_, name, _)) = field.as_mut() {
                            if !txt.is_empty() {
                                *name = Some(txt);
                            }
                        }
                    }
                    TextTarget::TabularName => {
                        if !txt.is_empty() {
                            out.tabular_sections.push(StructTabular {
                                name: txt,
                                attributes: Vec::new(),
                            });
                            cur_tab = Some(out.tabular_sections.len() - 1);
                            expecting_tabular_name = false;
                        }
                    }
                    TextTarget::TypeValue => {
                        if let Some((_, _, types)) = field.as_mut() {
                            if !txt.is_empty() {
                                types.push(txt);
                            }
                        }
                    }
                    TextTarget::None => {}
                    // RegisterRef в структурном парсере не возникает
                    // (RegisterRecords обрабатывает только parse_object_attributes_xml).
                    TextTarget::RegisterRef => {}
                }
                text_target = TextTarget::None;
            }
            Ok(Event::End(e)) => {
                let raw = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let local = local_name(&raw);
                match local.as_str() {
                    "Attribute" | "Dimension" | "Resource" | "EnumValue" => {
                        if let Some((kind, Some(name), types)) = field.take() {
                            if !name.is_empty() {
                                if kind == "EnumValue" {
                                    // B2: значение перечисления — только имя, без типа.
                                    out.enum_values.push(name);
                                } else {
                                    let f = StructField {
                                        name,
                                        type_str: pretty_types(&types),
                                    };
                                    match kind.as_str() {
                                        "Dimension" => out.dimensions.push(f),
                                        "Resource" => out.resources.push(f),
                                        _ => match cur_tab {
                                            Some(i) => out.tabular_sections[i].attributes.push(f),
                                            None => out.attributes.push(f),
                                        },
                                    }
                                }
                            }
                        }
                        in_type = false;
                    }
                    "TabularSection" => {
                        cur_tab = None;
                    }
                    _ => {
                        if raw == "Type" {
                            in_type = false;
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "object XML: ошибка парсинга на позиции {}: {}",
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

/// Склеить типы поля в человекочитаемую 1С-строку (составной → через ` | `).
fn pretty_types(types: &[String]) -> String {
    if types.is_empty() {
        return "—".to_string();
    }
    let mut parts: Vec<String> = types.iter().map(|t| pretty_one_type(t)).collect();
    parts.dedup();
    parts.join(" | ")
}

/// Один тип `<v8:Type>` → 1С-нотация. Примитивы и ссылки переводятся,
/// прочее отдаётся как есть (без префикса схемы).
fn pretty_one_type(t: &str) -> String {
    let t = t.trim();
    match t {
        "xs:string" => return "Строка".to_string(),
        "xs:decimal" => return "Число".to_string(),
        "xs:boolean" => return "Булево".to_string(),
        "xs:dateTime" | "xs:date" => return "Дата".to_string(),
        _ => {}
    }
    if let Some(rest) = t.strip_prefix("cfg:") {
        if let Some(dt) = rest.strip_prefix("DefinedType.") {
            return format!("ОпределяемыйТип.{}", dt);
        }
        if let Some((kind_ref, name)) = rest.split_once('.') {
            if let Some(kind) = kind_ref.strip_suffix("Ref") {
                return format!("{}.{}", ru_ref_kind(kind), name);
            }
        } else if let Some(kind) = rest.strip_suffix("Ref") {
            return ru_ref_kind(kind);
        }
        return rest.to_string();
    }
    if let Some(rest) = t.strip_prefix("v8:") {
        return rest.to_string();
    }
    t.to_string()
}

/// `Catalog` → `СправочникСсылка` и т.д.; неизвестное — `<Kind>Ссылка`.
fn ru_ref_kind(kind: &str) -> String {
    match kind {
        "Catalog" => "СправочникСсылка",
        "Document" => "ДокументСсылка",
        "Enum" => "ПеречислениеСсылка",
        "ChartOfCharacteristicTypes" => "ПланВидовХарактеристикСсылка",
        "ChartOfAccounts" => "ПланСчетовСсылка",
        "ChartOfCalculationTypes" => "ПланВидовРасчетаСсылка",
        "ExchangePlan" => "ПланОбменаСсылка",
        "BusinessProcess" => "БизнесПроцессСсылка",
        "Task" => "ЗадачаСсылка",
        "Any" => "ЛюбаяСсылка",
        other => return format!("{}Ссылка", other),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_from_unions_base_and_extension() {
        // base: Контрагент + ТЧ Товары{Номенклатура}.
        let mut base = ObjectStructure {
            attributes: vec![StructField {
                name: "Контрагент".into(),
                type_str: "СправочникСсылка.Контрагенты".into(),
            }],
            tabular_sections: vec![StructTabular {
                name: "Товары".into(),
                attributes: vec![StructField {
                    name: "Номенклатура".into(),
                    type_str: "СправочникСсылка.Номенклатура".into(),
                }],
            }],
            ..Default::default()
        };
        // extension: одноимённый Контрагент (другой тип — base должен победить),
        // новый реквизит УОП_Поле, и доп. реквизит в ТЧ Товары.
        let ext = ObjectStructure {
            attributes: vec![
                StructField { name: "Контрагент".into(), type_str: "ПроизвольнаяСсылка".into() },
                StructField { name: "УОП_Поле".into(), type_str: "Дата".into() },
            ],
            tabular_sections: vec![StructTabular {
                name: "Товары".into(),
                attributes: vec![StructField { name: "УОП_ТЧПоле".into(), type_str: "Число".into() }],
            }],
            ..Default::default()
        };
        base.merge_from(&ext);
        // 2 реквизита шапки: базовый + добавленный расширением.
        assert_eq!(base.attributes.len(), 2);
        // base-версия типа одноимённого реквизита сохранена.
        assert_eq!(base.attributes[0].type_str, "СправочникСсылка.Контрагенты");
        assert_eq!(base.attributes[1].name, "УОП_Поле");
        // ТЧ Товары слита: 2 реквизита (base + расширение), не задвоена.
        assert_eq!(base.tabular_sections.len(), 1);
        assert_eq!(base.tabular_sections[0].attributes.len(), 2);
    }

    #[test]
    fn classify_concrete_ref() {
        assert_eq!(
            classify_type("cfg:CatalogRef.Контрагенты"),
            Some(("Catalog.Контрагенты".to_string(), false))
        );
        assert_eq!(
            classify_type("cfg:DocumentRef.РеализацияТоваровУслуг"),
            Some(("Document.РеализацияТоваровУслуг".to_string(), false))
        );
        assert_eq!(
            classify_type("cfg:EnumRef.СтавкиНДС"),
            Some(("Enum.СтавкиНДС".to_string(), false))
        );
        assert_eq!(
            classify_type("cfg:ChartOfCharacteristicTypesRef.ВидыСубконто"),
            Some(("ChartOfCharacteristicTypes.ВидыСубконто".to_string(), false))
        );
    }

    #[test]
    fn classify_universal_and_defined() {
        assert_eq!(classify_type("cfg:AnyRef"), Some(("*AnyRef".to_string(), true)));
        assert_eq!(
            classify_type("cfg:CatalogRef"),
            Some(("*CatalogRef".to_string(), true))
        );
        assert_eq!(
            classify_type("cfg:DocumentRef"),
            Some(("*DocumentRef".to_string(), true))
        );
        assert_eq!(
            classify_type("cfg:DefinedType.Организация"),
            Some(("*DefinedType.Организация".to_string(), true))
        );
    }

    #[test]
    fn classify_primitives_are_none() {
        assert_eq!(classify_type("xs:string"), None);
        assert_eq!(classify_type("xs:decimal"), None);
        assert_eq!(classify_type("xs:boolean"), None);
        assert_eq!(classify_type("v8:StandardPeriod"), None);
    }

    #[test]
    fn parses_catalog_attributes_with_composite() {
        // Реальный фрагмент УТ: Catalog с обычным и составным реквизитом.
        let xml = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Catalog uuid="root">
    <Properties><Name>КлючиАналитики</Name></Properties>
    <ChildObjects>
      <Attribute uuid="a1">
        <Properties>
          <Name>Поставщик</Name>
          <Type><v8:Type>cfg:CatalogRef.Партнеры</v8:Type></Type>
        </Properties>
      </Attribute>
      <Attribute uuid="a2">
        <Properties>
          <Name>Контрагент</Name>
          <Type>
            <v8:Type>cfg:CatalogRef.Организации</v8:Type>
            <v8:Type>cfg:CatalogRef.Контрагенты</v8:Type>
          </Type>
        </Properties>
      </Attribute>
      <Attribute uuid="a3">
        <Properties>
          <Name>КодСтроки</Name>
          <Type><v8:Type>xs:decimal</v8:Type></Type>
        </Properties>
      </Attribute>
    </ChildObjects>
  </Catalog>
</MetaDataObject>"#;
        let edges = parse_object_attributes_xml(xml).unwrap();
        // Поставщик (1) + Контрагент составной (2) = 3 ребра, КодСтроки (примитив) пропущен.
        assert_eq!(edges.len(), 3, "ожидаем 3 ребра, получили {:?}", edges);

        let supplier: Vec<_> = edges.iter().filter(|e| e.from_path == "Поставщик").collect();
        assert_eq!(supplier.len(), 1);
        assert_eq!(supplier[0].to_object, "Catalog.Партнеры");
        assert_eq!(supplier[0].link_kind, "attr");
        assert!(!supplier[0].is_composite);

        let counterparty: Vec<_> = edges.iter().filter(|e| e.from_path == "Контрагент").collect();
        assert_eq!(counterparty.len(), 2);
        assert!(counterparty.iter().all(|e| e.is_composite));
        let targets: Vec<&str> = counterparty.iter().map(|e| e.to_object.as_str()).collect();
        assert!(targets.contains(&"Catalog.Организации"));
        assert!(targets.contains(&"Catalog.Контрагенты"));

        assert!(edges.iter().all(|e| e.from_path != "КодСтроки"), "примитив не должен давать ребро");
    }

    #[test]
    fn parses_register_dimensions() {
        // Регистр: измерения (Dimension) ссылочные, ресурс (Resource) числовой.
        let xml = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <AccumulationRegister uuid="root">
    <Properties><Name>ТоварыНаСкладах</Name></Properties>
    <ChildObjects>
      <Resource uuid="r1">
        <Properties><Name>ВНаличии</Name>
          <Type><v8:Type>xs:decimal</v8:Type></Type>
        </Properties>
      </Resource>
      <Dimension uuid="d1">
        <Properties><Name>Номенклатура</Name>
          <Type><v8:Type>cfg:CatalogRef.Номенклатура</v8:Type></Type>
        </Properties>
      </Dimension>
      <Dimension uuid="d2">
        <Properties><Name>Склад</Name>
          <Type><v8:Type>cfg:CatalogRef.Склады</v8:Type></Type>
        </Properties>
      </Dimension>
    </ChildObjects>
  </AccumulationRegister>
</MetaDataObject>"#;
        let edges = parse_object_attributes_xml(xml).unwrap();
        assert_eq!(edges.len(), 2, "две ссылочные размерности, ресурс числовой пропущен: {:?}", edges);
        assert!(edges.iter().all(|e| e.link_kind == "register_dim"));
        let nom = edges.iter().find(|e| e.from_path == "Номенклатура").unwrap();
        assert_eq!(nom.to_object, "Catalog.Номенклатура");
    }

    #[test]
    fn parses_tabular_section() {
        // Реквизит табличной части → from_path = "<ТЧ>.<Реквизит>".
        let xml = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Document uuid="root">
    <Properties><Name>РеализацияТоваровУслуг</Name></Properties>
    <ChildObjects>
      <Attribute uuid="a1">
        <Properties><Name>Контрагент</Name>
          <Type><v8:Type>cfg:CatalogRef.Контрагенты</v8:Type></Type>
        </Properties>
      </Attribute>
      <TabularSection uuid="ts1">
        <Properties><Name>Товары</Name></Properties>
        <ChildObjects>
          <Attribute uuid="a2">
            <Properties><Name>Номенклатура</Name>
              <Type><v8:Type>cfg:CatalogRef.Номенклатура</v8:Type></Type>
            </Properties>
          </Attribute>
        </ChildObjects>
      </TabularSection>
    </ChildObjects>
  </Document>
</MetaDataObject>"#;
        let edges = parse_object_attributes_xml(xml).unwrap();
        assert_eq!(edges.len(), 2, "{:?}", edges);

        let head = edges.iter().find(|e| e.from_path == "Контрагент").unwrap();
        assert_eq!(head.link_kind, "attr");
        assert_eq!(head.to_object, "Catalog.Контрагенты");

        let tab = edges.iter().find(|e| e.from_path == "Товары.Номенклатура").unwrap();
        assert_eq!(tab.link_kind, "tabular_attr");
        assert_eq!(tab.to_object, "Catalog.Номенклатура");
    }

    #[test]
    fn parses_register_records() {
        // <RegisterRecords> документа → рёбра recorder (документ → регистр).
        // Реквизит шапки даёт обычное attr-ребро и не путается с recorder.
        let xml = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core"
                xmlns:xr="http://v8.1c.ru/8.3/xcf/readable"
                xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <Document uuid="root">
    <Properties>
      <Name>РеализацияТоваровУслуг</Name>
      <RegisterRecords>
        <xr:Item xsi:type="xr:MDObjectRef">AccumulationRegister.ТоварыНаСкладах</xr:Item>
        <xr:Item xsi:type="xr:MDObjectRef">AccumulationRegister.Продажи</xr:Item>
        <xr:Item xsi:type="xr:MDObjectRef">AccountingRegister.Хозрасчетный</xr:Item>
      </RegisterRecords>
    </Properties>
    <ChildObjects>
      <Attribute uuid="a1">
        <Properties><Name>Контрагент</Name>
          <Type><v8:Type>cfg:CatalogRef.Контрагенты</v8:Type></Type>
        </Properties>
      </Attribute>
    </ChildObjects>
  </Document>
</MetaDataObject>"#;
        let edges = parse_object_attributes_xml(xml).unwrap();

        let recorders: Vec<_> = edges.iter().filter(|e| e.link_kind == "recorder").collect();
        assert_eq!(recorders.len(), 3, "три регистра-приёмника: {:?}", edges);
        let targets: Vec<&str> = recorders.iter().map(|e| e.to_object.as_str()).collect();
        assert!(targets.contains(&"AccumulationRegister.ТоварыНаСкладах"));
        assert!(targets.contains(&"AccumulationRegister.Продажи"));
        assert!(targets.contains(&"AccountingRegister.Хозрасчетный"));
        // У recorder-ребра пустой from_path, не composite и не universal.
        assert!(recorders.iter().all(|e| e.from_path.is_empty()));
        assert!(recorders.iter().all(|e| !e.is_composite && !e.is_universal));

        // Реквизит шапки по-прежнему даёт attr-ребро (recorder не ломает разбор).
        let attr = edges.iter().find(|e| e.from_path == "Контрагент").unwrap();
        assert_eq!(attr.link_kind, "attr");
        assert_eq!(attr.to_object, "Catalog.Контрагенты");
    }

    #[test]
    fn composite_cap_collapses_pathological_lists() {
        // > MAX_COMPOSITE_TARGETS конкретных типов → один *Multiple.
        let mut types = String::new();
        for i in 0..40 {
            types.push_str(&format!("<v8:Type>cfg:CatalogRef.Спр{}</v8:Type>\n", i));
        }
        let xml = format!(
            r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Catalog uuid="root"><ChildObjects>
    <Attribute uuid="a1"><Properties><Name>МногоТипов</Name>
      <Type>{}</Type>
    </Properties></Attribute>
  </ChildObjects></Catalog>
</MetaDataObject>"#,
            types
        );
        let edges = parse_object_attributes_xml(&xml).unwrap();
        assert_eq!(edges.len(), 1, "патологический перечень схлопнут в один узел");
        assert_eq!(edges[0].to_object, "*Multiple");
        assert!(edges[0].is_universal);
    }

    #[test]
    fn parses_enum_values() {
        // B2: <EnumValue> в ChildObjects перечисления → ObjectStructure.enum_values.
        let xml = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Enum uuid="root">
    <Properties><Name>ВедениеВзаиморасчетовПоДоговорам</Name></Properties>
    <ChildObjects>
      <EnumValue uuid="e1"><Properties><Name>ПоДоговоруВЦелом</Name></Properties></EnumValue>
      <EnumValue uuid="e2"><Properties><Name>ПоЗаказам</Name></Properties></EnumValue>
      <EnumValue uuid="e3"><Properties><Name>ПоСчетам</Name></Properties></EnumValue>
    </ChildObjects>
  </Enum>
</MetaDataObject>"#;
        let st = parse_object_structure_xml(xml).unwrap();
        assert_eq!(
            st.enum_values,
            vec!["ПоДоговоруВЦелом", "ПоЗаказам", "ПоСчетам"]
        );
        assert!(!st.is_empty(), "перечисление со значениями не пусто");
        assert!(st.attributes.is_empty() && st.tabular_sections.is_empty());

        // to_json: базовые секции пусты, но присутствуют; enum_values заполнен.
        let j = st.to_json();
        let obj = j.as_object().unwrap();
        assert!(obj.get("attributes").unwrap().as_array().unwrap().is_empty());
        assert_eq!(obj.get("enum_values").unwrap().as_array().unwrap().len(), 3);
    }

    #[test]
    fn to_json_always_emits_base_sections() {
        // B1: даже при пустых секциях ключи attributes/dimensions/resources/
        // tabular_sections присутствуют — агент видит форму, не уходит в XML.
        let xml = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Catalog uuid="root">
    <Properties><Name>Контрагенты</Name></Properties>
    <ChildObjects>
      <Attribute uuid="a1"><Properties><Name>ИНН</Name>
        <Type><v8:Type>xs:string</v8:Type></Type>
      </Properties></Attribute>
    </ChildObjects>
  </Catalog>
</MetaDataObject>"#;
        let st = parse_object_structure_xml(xml).unwrap();
        let j = st.to_json();
        let obj = j.as_object().unwrap();
        for key in ["attributes", "dimensions", "resources", "tabular_sections"] {
            assert!(obj.contains_key(key), "ключ {} должен присутствовать всегда", key);
            assert!(obj.get(key).unwrap().is_array());
        }
        assert_eq!(obj.get("attributes").unwrap().as_array().unwrap().len(), 1);
        assert!(obj.get("dimensions").unwrap().as_array().unwrap().is_empty());
        // enum_values НЕ эмитится для не-перечисления.
        assert!(!obj.contains_key("enum_values"));
    }

    #[test]
    fn parses_predefined_items() {
        // C2: <Item>/<Name> из Predefined.xml → имена предопределённых.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<PredefinedData xmlns="http://v8.1c.ru/8.3/xcf/predef"
                xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                xsi:type="CatalogPredefinedItems" version="2.20">
    <Item id="d05404a0">
        <Name>Новый</Name>
        <Code>000000001</Code>
        <Description>Новый</Description>
        <IsFolder>false</IsFolder>
    </Item>
    <Item id="abc123">
        <Name>Брак</Name>
        <Code>000000002</Code>
        <Description>Брак</Description>
        <IsFolder>false</IsFolder>
    </Item>
</PredefinedData>"#;
        let names = parse_predefined_xml(xml);
        assert_eq!(names, vec!["Новый", "Брак"]);
    }
}
