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
    /// Текст `<xr:Item>` внутри `<Owners>` — владелец подчинённого справочника.
    OwnerRef,
    /// W13: `<v8:Type>`/`<v8:TypeSet>` внутри КОРНЕВОГО `<Type>` (ПВХ/константа).
    RootTypeValue,
    /// W11: `<v8:lang>` внутри `<Synonym>` текущего поля.
    FieldSynLang,
    /// W11: `<v8:content>` внутри `<Synonym>` текущего поля.
    FieldSynContent,
    /// W9: `<FillChecking>` текущего поля (ShowError → required).
    FieldFillChecking,
    /// W8: скалярное свойство шапки из белого списка (имя — в `cur_header_prop`).
    HeaderProp,
    /// Значение свойства проведения документа (Posting / RegisterRecordsDeletion
    /// и т.п.); имя свойства лежит в `cur_posting_prop`.
    PostingProp,
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
    // Внутри <Owners> — список владельцев подчинённого справочника.
    let mut in_owners = false;
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
                    "Owners" => {
                        // Владельцы подчинённого справочника: <xr:Item> внутри —
                        // полные имена объектов-владельцев (Catalog.X и т.п.).
                        in_owners = true;
                    }
                    "Item" if in_owners => {
                        text_target = TextTarget::OwnerRef;
                    }
                    _ => {
                        // Различаем контейнер <Type> и элемент <v8:Type>.
                        // local_name у обоих == "Type" — смотрим сырое имя.
                        if raw == "Type" {
                            if field.is_some() {
                                in_type = true;
                            }
                        } else if raw.ends_with(":Type") || raw.ends_with(":TypeSet") {
                            // <v8:Type> — значение типа. <v8:TypeSet> — тип-набор
                            // (cfg:DefinedType.X): до 0.32 не ловился → тип «—»
                            // и потеря ребра DefinedType (W2).
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
                    TextTarget::OwnerRef => {
                        // Подчинённый справочник → владелец: ребро owner.
                        // Цель уже каноническая (Catalog.X / ExchangePlan.X).
                        if !txt.is_empty() {
                            out.push(DataLinkEdge {
                                from_path: String::new(),
                                to_object: txt,
                                link_kind: "owner",
                                is_composite: false,
                                is_universal: false,
                            });
                        }
                    }
                    // Прочие цели (свойства шапки/проведения, синонимы,
                    // FillChecking, корневой Type) в парсере связей данных не
                    // возникают — их обрабатывает parse_object_structure_xml.
                    _ => {}
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
                    "Owners" => {
                        in_owners = false;
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
    /// Синоним (ru) реквизита — UI-подпись поля (W11). None, если не задан
    /// или совпадает с именем (не дублируем).
    pub synonym: Option<String>,
    /// Обязательность заполнения: `<FillChecking>ShowError</FillChecking>` (W9).
    pub required: bool,
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
    /// Свойства проведения документа из корневого `<Properties>`:
    /// (имя свойства, значение). Например `("Posting","Allow")`,
    /// `("RegisterRecordsDeletion","AutoDeleteOff")`. Только у Document;
    /// у прочих объектов пусто. Источник — теги `Posting` / `RealTimePosting`
    /// / `RegisterRecordsDeletion` / `RegisterRecordsWritingOnPost`.
    pub posting: Vec<(String, String)>,
    /// Владельцы подчинённого справочника (`<Owners>` в `<Properties>`),
    /// канонические имена (`Catalog.Партнеры`). Пусто у неподчинённых. W6.
    pub owners: Vec<String>,
    /// Тип значения характеристик ПВХ / тип константы — корневой `<Type>`
    /// в `<Properties>` (W13). 1С-нотация (`СправочникСсылка.Организации`).
    /// Для ПВХ это список ДОСТУПНЫХ АНАЛИТИК.
    pub value_types: Vec<String>,
    /// Скалярные свойства шапки объекта по белому списку (W8):
    /// периодичность/режим записи ИР, вид регистра накопления, нумерация
    /// документа, иерархия/длины кодов справочника. (имя, значение).
    pub properties: Vec<(String, String)>,
    /// Синонимы (ru) значений перечисления (W11): (имя_значения, синоним).
    /// Только значения, у которых синоним задан и отличается от имени.
    pub enum_synonyms: Vec<(String, String)>,
    /// Команды объекта (W4): (имя, синоним-UI-подпись). Синоним — Some
    /// только когда задан и отличается от имени. Из `<Command>` в
    /// `<ChildObjects>` («Создать на основании», печатные формы и т.п.).
    pub commands: Vec<(String, Option<String>)>,
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
            && self.posting.is_empty()
            && self.owners.is_empty()
            && self.value_types.is_empty()
            && self.properties.is_empty()
            && self.commands.is_empty()
    }

    /// Сериализовать в JSON для `attributes_json` (пустые секции опускаем).
    pub fn to_json(&self) -> Value {
        // W9/W11: synonym и required — только когда несут информацию
        // (синоним отличен от имени; required=true), чтобы не раздувать блоб.
        let field = |f: &StructField| {
            let mut m = serde_json::Map::new();
            m.insert("name".into(), Value::String(f.name.clone()));
            m.insert("type".into(), Value::String(f.type_str.clone()));
            if let Some(s) = &f.synonym {
                if !s.is_empty() && s != &f.name {
                    m.insert("synonym".into(), Value::String(s.clone()));
                }
            }
            if f.required {
                m.insert("required".into(), Value::Bool(true));
            }
            Value::Object(m)
        };
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
        // WS-1: posting — свойства проведения документа (только у Document).
        // Объект {имя_свойства: значение}, чтобы агент видел поведение при
        // проведении/отмене (RegisterRecordsDeletion=AutoDeleteOff и т.п.)
        // без ухода в XML.
        if !self.posting.is_empty() {
            let mut pm = serde_json::Map::new();
            for (k, v) in &self.posting {
                pm.insert(k.clone(), Value::String(v.clone()));
            }
            map.insert("posting".into(), Value::Object(pm));
        }
        // W6: owners — владельцы подчинённого справочника (если есть).
        if !self.owners.is_empty() {
            map.insert(
                "owners".into(),
                Value::Array(
                    self.owners
                        .iter()
                        .map(|v| Value::String(v.clone()))
                        .collect(),
                ),
            );
        }
        // W13: value_types — тип значения характеристик ПВХ / константы.
        if !self.value_types.is_empty() {
            map.insert(
                "value_types".into(),
                Value::Array(
                    self.value_types
                        .iter()
                        .map(|v| Value::String(v.clone()))
                        .collect(),
                ),
            );
        }
        // W8: properties — скалярные свойства шапки (периодичность ИР,
        // режим записи, нумерация документа и т.п.) из белого списка.
        if !self.properties.is_empty() {
            let mut pm = serde_json::Map::new();
            for (k, v) in &self.properties {
                pm.insert(k.clone(), Value::String(v.clone()));
            }
            map.insert("properties".into(), Value::Object(pm));
        }
        // W11: enum_synonyms — UI-подписи значений перечисления
        // (отдельной картой, чтобы не ломать формат enum_values: [имена]).
        if !self.enum_synonyms.is_empty() {
            let mut em = serde_json::Map::new();
            for (k, v) in &self.enum_synonyms {
                em.insert(k.clone(), Value::String(v.clone()));
            }
            map.insert("enum_synonyms".into(), Value::Object(em));
        }
        // W4: commands — команды объекта (имя + UI-подпись, если отличается).
        if !self.commands.is_empty() {
            let cs: Vec<Value> = self
                .commands
                .iter()
                .map(|(name, syn)| {
                    let mut cm = serde_json::Map::new();
                    cm.insert("name".into(), Value::String(name.clone()));
                    if let Some(s) = syn {
                        cm.insert("synonym".into(), Value::String(s.clone()));
                    }
                    Value::Object(cm)
                })
                .collect();
            map.insert("commands".into(), Value::Array(cs));
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
        merge_names(&mut self.owners, &other.owners);
        merge_names(&mut self.value_types, &other.value_types);
        // posting: свойства проведения из other, которых ещё нет по имени
        // (base-приоритет — свойство документа обычно живёт в base-конфиге).
        for (k, v) in &other.posting {
            if !self.posting.iter().any(|(ek, _)| ek == k) {
                self.posting.push((k.clone(), v.clone()));
            }
        }
        // W8/W11: те же правила base-приоритета для свойств шапки и синонимов.
        for (k, v) in &other.properties {
            if !self.properties.iter().any(|(ek, _)| ek == k) {
                self.properties.push((k.clone(), v.clone()));
            }
        }
        for (k, v) in &other.enum_synonyms {
            if !self.enum_synonyms.iter().any(|(ek, _)| ek == k) {
                self.enum_synonyms.push((k.clone(), v.clone()));
            }
        }
        // W4: команды расширения, которых нет в base (union по имени).
        for (k, v) in &other.commands {
            if !self.commands.iter().any(|(ek, _)| ek == k) {
                self.commands.push((k.clone(), v.clone()));
            }
        }
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
/// Лёгкий парс ШАПКИ объекта: `meta_type` (корневой тег под `MetaDataObject`),
/// имя (`<Name>` в `<Properties>`) и синоним (`<Synonym>` ru-представление).
/// Прерывается на `<ChildObjects>` — свойства объекта идут ДО состава, читать
/// дальше незачем. Используется проходом `index_object_synonyms`, который
/// покрывает ВСЕ типы объектов (включая CommonModule/Constant/… без структуры
/// реквизитов, не входящие в `OBJECT_FOLDERS`). Возвращает `None`, если корневой
/// тег/имя не распознаны. Синоним: приоритет у `<v8:lang>ru`, иначе первый
/// непустой `<v8:content>`.
pub fn parse_object_header_xml(content: &str) -> Option<(String, String, Option<String>)> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    let mut depth = 0i32;
    let mut meta_type: Option<String> = None;
    let mut name: Option<String> = None;
    let mut synonym: Option<String> = None;

    // Состояние парса ПЕРВОГО <Synonym> объекта (синоним самого объекта).
    let mut in_synonym = false;
    let mut synonym_done = false;
    let mut cur_lang: Option<String> = None;
    let mut want_lang = false;
    let mut want_content = false;
    let mut want_name = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                depth += 1;
                let local = local_name(&String::from_utf8_lossy(e.name().as_ref()));
                // Корневой дочерний тег MetaDataObject (depth 2) — это meta_type.
                if meta_type.is_none() && depth == 2 && local != "MetaDataObject" {
                    meta_type = Some(local.clone());
                }
                // Состав объекта начался — свойства (Name/Synonym) уже позади.
                if local == "ChildObjects" {
                    break;
                }
                if local == "Name" && name.is_none() && !in_synonym {
                    want_name = true;
                } else if local == "Synonym" && !synonym_done && !in_synonym {
                    in_synonym = true;
                } else if in_synonym && local == "lang" {
                    want_lang = true;
                } else if in_synonym && local == "content" {
                    want_content = true;
                }
            }
            Ok(Event::Text(t)) => {
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
                } else if want_lang {
                    cur_lang = if txt.is_empty() { None } else { Some(txt) };
                    want_lang = false;
                } else if want_content {
                    if !txt.is_empty() {
                        if cur_lang.as_deref() == Some("ru") {
                            synonym = Some(txt); // ru имеет приоритет
                        } else if synonym.is_none() {
                            synonym = Some(txt); // иначе — первое непустое
                        }
                    }
                    cur_lang = None;
                    want_content = false;
                }
            }
            Ok(Event::End(e)) => {
                depth -= 1;
                let local = local_name(&String::from_utf8_lossy(e.name().as_ref()));
                if local == "Synonym" && in_synonym {
                    in_synonym = false;
                    synonym_done = true;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
        if meta_type.is_some() && name.is_some() && synonym_done {
            break;
        }
    }

    match (meta_type, name) {
        (Some(mt), Some(nm)) => Some((mt, nm, synonym)),
        _ => None,
    }
}

pub fn parse_object_structure_xml(content: &str) -> Result<ObjectStructure> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut out = ObjectStructure::default();
    let mut buf = Vec::new();

    // Текущее разбираемое поле (реквизит/измерение/ресурс/значение enum).
    struct FieldBuild {
        kind: String,
        name: Option<String>,
        types: Vec<String>,
        /// W11: синоним (ru-приоритет).
        synonym: Option<String>,
        /// W9: FillChecking == ShowError.
        required: bool,
    }

    // Индекс текущей табличной части (Some, пока мы внутри <TabularSection>).
    let mut cur_tab: Option<usize> = None;
    let mut expecting_tabular_name = false;
    let mut field: Option<FieldBuild> = None;
    let mut in_type = false;
    let mut text_target = TextTarget::None;
    // WS-1: имя свойства проведения, чей текст сейчас разбираем (Posting и т.п.).
    let mut cur_posting_prop: Option<String> = None;
    // W6: внутри <Owners> — список владельцев подчинённого справочника.
    let mut in_owners = false;
    // W8: имя свойства шапки из белого списка, чей текст сейчас разбираем.
    let mut cur_header_prop: Option<String> = None;
    // W13: внутри корневого <Type> (тип значения характеристик ПВХ/константы).
    let mut in_root_type = false;
    // Внутри <StandardAttributes> — стандартные атрибуты не разбираем.
    let mut in_std_attrs = false;
    // W11: внутри <Synonym> текущего поля; последний прочитанный <v8:lang>.
    let mut in_field_syn = false;
    let mut syn_lang: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let raw = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let local = local_name(&raw);
                match local.as_str() {
                    "TabularSection" => {
                        expecting_tabular_name = true;
                    }
                    // W4: Command разбирается тем же накопителем — у него те же
                    // <Name> и <Synonym>; типы/FillChecking у команд не встречаются.
                    "Attribute" | "Dimension" | "Resource" | "EnumValue" | "Command" => {
                        field = Some(FieldBuild {
                            kind: local,
                            name: None,
                            types: Vec::new(),
                            synonym: None,
                            required: false,
                        });
                    }
                    "Name" => {
                        if let Some(f) = field.as_ref() {
                            if f.name.is_none() {
                                text_target = TextTarget::FieldName;
                            }
                        } else if expecting_tabular_name {
                            text_target = TextTarget::TabularName;
                        }
                    }
                    // WS-1: свойства проведения документа в корневом <Properties>.
                    // Ловим только вне реквизита (field.is_none()) — эти теги
                    // платформенно-уникальны и не встречаются внутри <Attribute>.
                    "Posting" | "RealTimePosting" | "RegisterRecordsDeletion"
                    | "RegisterRecordsWritingOnPost" => {
                        if field.is_none() {
                            cur_posting_prop = Some(local.clone());
                            text_target = TextTarget::PostingProp;
                        }
                    }
                    // W6: владельцы подчинённого справочника.
                    "Owners" => {
                        in_owners = true;
                    }
                    "Item" if in_owners => {
                        text_target = TextTarget::OwnerRef;
                    }
                    "StandardAttributes" => {
                        in_std_attrs = true;
                    }
                    // W11: синоним поля (у ТЧ и корня свои Synonym — field=None).
                    "Synonym" if field.is_some() => {
                        in_field_syn = true;
                        syn_lang = None;
                    }
                    // W9: обязательность заполнения поля.
                    "FillChecking" if field.is_some() => {
                        text_target = TextTarget::FieldFillChecking;
                    }
                    // W8: скалярные свойства шапки из белого списка — вне
                    // реквизитов и стандартных атрибутов (там одноимённые теги).
                    "InformationRegisterPeriodicity" | "WriteMode" | "RegisterType"
                    | "NumberType" | "NumberLength" | "NumberPeriodicity"
                    | "CheckUnique" | "Autonumbering" | "Hierarchical"
                    | "CodeLength" | "DescriptionLength" | "Numerator" => {
                        if field.is_none() && !in_std_attrs {
                            cur_header_prop = Some(local.clone());
                            text_target = TextTarget::HeaderProp;
                        }
                    }
                    _ => {
                        if raw == "Type" {
                            if field.is_some() {
                                in_type = true;
                            } else if !in_std_attrs {
                                // W13: корневой <Type> — тип значения
                                // характеристик ПВХ / тип константы.
                                in_root_type = true;
                            }
                        } else if raw.ends_with(":Type") || raw.ends_with(":TypeSet") {
                            // <v8:TypeSet> — тип-набор (cfg:DefinedType.X), W2.
                            if field.is_some() && in_type {
                                text_target = TextTarget::TypeValue;
                            } else if in_root_type {
                                text_target = TextTarget::RootTypeValue;
                            }
                        } else if in_field_syn && raw.ends_with(":lang") {
                            text_target = TextTarget::FieldSynLang;
                        } else if in_field_syn && raw.ends_with(":content") {
                            text_target = TextTarget::FieldSynContent;
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
                            out.tabular_sections.push(StructTabular {
                                name: txt,
                                attributes: Vec::new(),
                            });
                            cur_tab = Some(out.tabular_sections.len() - 1);
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
                    TextTarget::PostingProp => {
                        if let Some(prop) = cur_posting_prop.take() {
                            if !txt.is_empty() {
                                out.posting.push((prop, txt));
                            }
                        }
                    }
                    // W6: владелец подчинённого справочника → секция owners.
                    TextTarget::OwnerRef => {
                        if !txt.is_empty() {
                            out.owners.push(txt);
                        }
                    }
                    // W13: тип значения характеристик / тип константы.
                    TextTarget::RootTypeValue => {
                        if !txt.is_empty() {
                            out.value_types.push(pretty_one_type(&txt));
                        }
                    }
                    // W11: синоним поля — ru-приоритет, иначе первый попавшийся.
                    TextTarget::FieldSynLang => {
                        syn_lang = Some(txt);
                    }
                    TextTarget::FieldSynContent => {
                        if let Some(f) = field.as_mut() {
                            if !txt.is_empty()
                                && (syn_lang.as_deref() == Some("ru") || f.synonym.is_none())
                            {
                                f.synonym = Some(txt);
                            }
                        }
                    }
                    // W9: FillChecking=ShowError → поле обязательно к заполнению.
                    TextTarget::FieldFillChecking => {
                        if let Some(f) = field.as_mut() {
                            f.required = txt == "ShowError";
                        }
                    }
                    // W8: скалярное свойство шапки из белого списка.
                    TextTarget::HeaderProp => {
                        if let Some(prop) = cur_header_prop.take() {
                            if !txt.is_empty() {
                                out.properties.push((prop, txt));
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
                    "Attribute" | "Dimension" | "Resource" | "EnumValue" | "Command" => {
                        if let Some(fb) = field.take() {
                            if let Some(name) = fb.name.filter(|n| !n.is_empty()) {
                                if fb.kind == "EnumValue" {
                                    // B2: значение перечисления — имя; синоним
                                    // (W11) — отдельной картой enum_synonyms.
                                    if let Some(s) = fb.synonym {
                                        if !s.is_empty() && s != name {
                                            out.enum_synonyms.push((name.clone(), s));
                                        }
                                    }
                                    out.enum_values.push(name);
                                } else if fb.kind == "Command" {
                                    // W4: команда объекта — имя + синоним
                                    // (только когда отличается от имени).
                                    let syn =
                                        fb.synonym.filter(|s| !s.is_empty() && s != &name);
                                    out.commands.push((name, syn));
                                } else {
                                    let f = StructField {
                                        name,
                                        type_str: pretty_types(&fb.types),
                                        synonym: fb.synonym,
                                        required: fb.required,
                                    };
                                    match fb.kind.as_str() {
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
                    "Owners" => {
                        in_owners = false;
                    }
                    "Synonym" => {
                        in_field_syn = false;
                    }
                    "StandardAttributes" => {
                        in_std_attrs = false;
                    }
                    _ => {
                        if raw == "Type" {
                            in_type = false;
                            in_root_type = false;
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
/// pub(crate): переиспользуется EDT-парсером (xml::edt_mdo) — нормализует
/// типы `.mdo` в тот же `cfg:`/`xs:` вид и зовёт эту же функцию.
pub(crate) fn pretty_types(types: &[String]) -> String {
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
    fn parse_header_extracts_meta_type_name_synonym_and_stops_at_childobjects() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.1/data/core">
  <Catalog uuid="abc">
    <Properties>
      <Name>Контрагенты</Name>
      <Synonym>
        <v8:item><v8:lang>ru</v8:lang><v8:content>Контрагенты (партнёры)</v8:content></v8:item>
      </Synonym>
      <Comment/>
    </Properties>
    <ChildObjects>
      <Attribute><Properties><Name>Поле</Name>
        <Synonym><v8:item><v8:lang>ru</v8:lang><v8:content>НЕ ЭТОТ</v8:content></v8:item></Synonym>
      </Properties></Attribute>
    </ChildObjects>
  </Catalog>
</MetaDataObject>"#;
        let (mt, name, syn) = parse_object_header_xml(xml).expect("header");
        assert_eq!(mt, "Catalog");
        assert_eq!(name, "Контрагенты");
        // Синоним именно ОБЪЕКТА, не вложенного реквизита (break на ChildObjects).
        assert_eq!(syn.as_deref(), Some("Контрагенты (партнёры)"));
    }

    #[test]
    fn parse_header_ru_priority_and_absent_synonym() {
        // en идёт перед ru — ru должен победить.
        let xml = r#"<MetaDataObject xmlns:v8="http://v8.1c.ru/8.1/data/core"><CommonModule><Properties><Name>ОбщийМодуль1</Name><Synonym><v8:item><v8:lang>en</v8:lang><v8:content>Common</v8:content></v8:item><v8:item><v8:lang>ru</v8:lang><v8:content>Общий модуль</v8:content></v8:item></Synonym></Properties></CommonModule></MetaDataObject>"#;
        let (mt, name, syn) = parse_object_header_xml(xml).expect("header");
        assert_eq!(mt, "CommonModule");
        assert_eq!(name, "ОбщийМодуль1");
        assert_eq!(syn.as_deref(), Some("Общий модуль"));

        // Объект без <Synonym> → synonym None (но meta_type/name есть).
        let xml2 = r#"<MetaDataObject><Constant><Properties><Name>Конст1</Name></Properties></Constant></MetaDataObject>"#;
        let (mt2, name2, syn2) = parse_object_header_xml(xml2).expect("header2");
        assert_eq!(mt2, "Constant");
        assert_eq!(name2, "Конст1");
        assert_eq!(syn2, None);
    }

    /// Тестовый конструктор поля без синонима/обязательности.
    fn sf(name: &str, type_str: &str) -> StructField {
        StructField {
            name: name.into(),
            type_str: type_str.into(),
            synonym: None,
            required: false,
        }
    }

    #[test]
    fn merge_from_unions_base_and_extension() {
        // base: Контрагент + ТЧ Товары{Номенклатура}.
        let mut base = ObjectStructure {
            attributes: vec![sf("Контрагент", "СправочникСсылка.Контрагенты")],
            tabular_sections: vec![StructTabular {
                name: "Товары".into(),
                attributes: vec![sf("Номенклатура", "СправочникСсылка.Номенклатура")],
            }],
            ..Default::default()
        };
        // extension: одноимённый Контрагент (другой тип — base должен победить),
        // новый реквизит УОП_Поле, и доп. реквизит в ТЧ Товары.
        let ext = ObjectStructure {
            attributes: vec![
                sf("Контрагент", "ПроизвольнаяСсылка"),
                sf("УОП_Поле", "Дата"),
            ],
            tabular_sections: vec![StructTabular {
                name: "Товары".into(),
                attributes: vec![sf("УОП_ТЧПоле", "Число")],
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
    fn parses_catalog_owners() {
        // W6: <Owners> подчинённого справочника → рёбра owner (подчинённый →
        // владелец) в графе данных и секция owners в структуре объекта.
        let xml = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core"
                xmlns:xr="http://v8.1c.ru/8.3/xcf/readable"
                xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <Catalog uuid="root">
    <Properties>
      <Name>ЗадачиПроектов</Name>
      <Owners>
        <xr:Item xsi:type="xr:MDObjectRef">Catalog.Претензии</xr:Item>
        <xr:Item xsi:type="xr:MDObjectRef">Catalog.СделкиСКлиентами</xr:Item>
      </Owners>
    </Properties>
    <ChildObjects>
      <Attribute uuid="a1">
        <Properties><Name>Исполнитель</Name>
          <Type><v8:Type>cfg:CatalogRef.Пользователи</v8:Type></Type>
        </Properties>
      </Attribute>
    </ChildObjects>
  </Catalog>
</MetaDataObject>"#;
        // Граф данных: рёбра owner с пустым from_path.
        let edges = parse_object_attributes_xml(xml).unwrap();
        let owners: Vec<_> = edges.iter().filter(|e| e.link_kind == "owner").collect();
        assert_eq!(owners.len(), 2, "два владельца: {:?}", edges);
        let targets: Vec<&str> = owners.iter().map(|e| e.to_object.as_str()).collect();
        assert!(targets.contains(&"Catalog.Претензии"));
        assert!(targets.contains(&"Catalog.СделкиСКлиентами"));
        assert!(owners.iter().all(|e| e.from_path.is_empty()));
        assert!(owners.iter().all(|e| !e.is_composite && !e.is_universal));
        // Обычный реквизит не путается с владельцами.
        let attr = edges.iter().find(|e| e.from_path == "Исполнитель").unwrap();
        assert_eq!(attr.link_kind, "attr");

        // Структура объекта: секция owners в attributes_json.
        let st = parse_object_structure_xml(xml).unwrap();
        assert_eq!(st.owners, vec!["Catalog.Претензии", "Catalog.СделкиСКлиентами"]);
        let js = st.to_json();
        let arr: Vec<&str> = js["owners"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(arr, vec!["Catalog.Претензии", "Catalog.СделкиСКлиентами"]);
    }

    #[test]
    fn parses_defined_type_typeset() {
        // W2: тип реквизита, сериализованный как <v8:TypeSet>cfg:DefinedType.X
        // (а не <v8:Type>), должен давать тип «ОпределяемыйТип.X» в структуре
        // и ребро *DefinedType.X в графе данных (раньше: тип «—», ребра нет).
        let xml = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Catalog uuid="root">
    <Properties><Name>Организации</Name></Properties>
    <ChildObjects>
      <Attribute uuid="a1">
        <Properties><Name>ИНН</Name>
          <Type><v8:TypeSet>cfg:DefinedType.ИНН</v8:TypeSet></Type>
        </Properties>
      </Attribute>
      <Attribute uuid="a2">
        <Properties><Name>ДокументОснование</Name>
          <Type><v8:TypeSet>cfg:DefinedType.ОснованиеСчетФактураВыданный</v8:TypeSet></Type>
        </Properties>
      </Attribute>
    </ChildObjects>
  </Catalog>
</MetaDataObject>"#;
        // Структура: тип читается, не «—».
        let st = parse_object_structure_xml(xml).unwrap();
        let inn = st.attributes.iter().find(|f| f.name == "ИНН").unwrap();
        assert_eq!(inn.type_str, "ОпределяемыйТип.ИНН");
        // Граф данных: ребро на DefinedType-терминал (universal).
        let edges = parse_object_attributes_xml(xml).unwrap();
        let e = edges.iter().find(|e| e.from_path == "ИНН").unwrap();
        assert_eq!(e.to_object, "*DefinedType.ИНН");
        assert!(e.is_universal);
        let e2 = edges.iter().find(|e| e.from_path == "ДокументОснование").unwrap();
        assert_eq!(e2.to_object, "*DefinedType.ОснованиеСчетФактураВыданный");
    }

    #[test]
    fn parses_header_props_fill_checking_synonyms_and_value_types() {
        // W8: свойства шапки (периодичность ИР, режим записи); W9: FillChecking
        // → required; W11: синонимы реквизитов и значений enum; W13: корневой
        // <Type> ПВХ → value_types. Стандартные атрибуты не подмешиваются.
        let xml = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core"
                xmlns:xr="http://v8.1c.ru/8.3/xcf/readable">
  <InformationRegister uuid="root">
    <Properties>
      <Name>ЦеныНоменклатуры25</Name>
      <Type>
        <v8:Type>cfg:CatalogRef.Организации</v8:Type>
        <v8:TypeSet>cfg:DefinedType.ИНН</v8:TypeSet>
      </Type>
      <InformationRegisterPeriodicity>Second</InformationRegisterPeriodicity>
      <WriteMode>RecorderSubordinate</WriteMode>
      <StandardAttributes>
        <xr:StandardAttribute name="Active">
          <xr:FillChecking>ShowError</xr:FillChecking>
          <xr:Synonym>
            <v8:item><v8:lang>ru</v8:lang><v8:content>Активность</v8:content></v8:item>
          </xr:Synonym>
        </xr:StandardAttribute>
      </StandardAttributes>
    </Properties>
    <ChildObjects>
      <Dimension uuid="d1">
        <Properties><Name>Номенклатура</Name>
          <Synonym>
            <v8:item><v8:lang>en</v8:lang><v8:content>Product</v8:content></v8:item>
            <v8:item><v8:lang>ru</v8:lang><v8:content>Товар</v8:content></v8:item>
          </Synonym>
          <Type><v8:Type>cfg:CatalogRef.Номенклатура</v8:Type></Type>
          <FillChecking>ShowError</FillChecking>
        </Properties>
      </Dimension>
      <Resource uuid="r1">
        <Properties><Name>Цена</Name>
          <Type><v8:Type>xs:decimal</v8:Type></Type>
          <FillChecking>DontCheck</FillChecking>
        </Properties>
      </Resource>
      <EnumValue uuid="e1">
        <Properties><Name>ЗакупкаПоИмпорту</Name>
          <Synonym>
            <v8:item><v8:lang>ru</v8:lang><v8:content>Ввоз из ЕАЭС</v8:content></v8:item>
          </Synonym>
        </Properties>
      </EnumValue>
    </ChildObjects>
  </InformationRegister>
</MetaDataObject>"#;
        let st = parse_object_structure_xml(xml).unwrap();
        // W8: свойства шапки по белому списку.
        assert!(st
            .properties
            .contains(&("InformationRegisterPeriodicity".into(), "Second".into())));
        assert!(st.properties.contains(&("WriteMode".into(), "RecorderSubordinate".into())));
        // W13: корневой Type → value_types (включая TypeSet/DefinedType).
        assert_eq!(
            st.value_types,
            vec!["СправочникСсылка.Организации", "ОпределяемыйТип.ИНН"]
        );
        // W9 + W11: измерение — required, ru-синоним при двух языках.
        let dim = &st.dimensions[0];
        assert_eq!(dim.name, "Номенклатура");
        assert!(dim.required);
        assert_eq!(dim.synonym.as_deref(), Some("Товар"));
        // DontCheck — не required.
        let res = &st.resources[0];
        assert!(!res.required);
        // W11: синоним значения перечисления — в enum_synonyms.
        assert_eq!(st.enum_values, vec!["ЗакупкаПоИмпорту"]);
        assert_eq!(
            st.enum_synonyms,
            vec![("ЗакупкаПоИмпорту".to_string(), "Ввоз из ЕАЭС".to_string())]
        );
        // Стандартные атрибуты не попали ни в поля, ни в свойства.
        assert!(st.attributes.is_empty());

        // JSON: required/synonym у поля, секции properties/value_types/enum_synonyms.
        let js = st.to_json();
        assert_eq!(js["dimensions"][0]["required"], true);
        assert_eq!(js["dimensions"][0]["synonym"], "Товар");
        assert!(js["resources"][0].get("required").is_none());
        assert_eq!(js["properties"]["WriteMode"], "RecorderSubordinate");
        assert_eq!(js["enum_synonyms"]["ЗакупкаПоИмпорту"], "Ввоз из ЕАЭС");
    }

    #[test]
    fn owners_absent_for_regular_catalog() {
        // Справочник без <Owners> (или с <Owners/>) — секции owners нет.
        let xml = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Catalog uuid="root">
    <Properties>
      <Name>Партнеры</Name>
      <Owners/>
    </Properties>
    <ChildObjects>
      <Attribute uuid="a1">
        <Properties><Name>Менеджер</Name>
          <Type><v8:Type>cfg:CatalogRef.Пользователи</v8:Type></Type>
        </Properties>
      </Attribute>
    </ChildObjects>
  </Catalog>
</MetaDataObject>"#;
        let edges = parse_object_attributes_xml(xml).unwrap();
        assert!(edges.iter().all(|e| e.link_kind != "owner"));
        let st = parse_object_structure_xml(xml).unwrap();
        assert!(st.owners.is_empty());
        assert!(st.to_json().get("owners").is_none());
    }

    #[test]
    fn parses_object_commands() {
        // W4: команды объекта из <ChildObjects>/<Command> — имя + синоним.
        let xml = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Document uuid="root">
    <Properties>
      <Name>ЗаказКлиента</Name>
    </Properties>
    <ChildObjects>
      <Attribute uuid="a1">
        <Properties><Name>Контрагент</Name>
          <Type><v8:Type>cfg:CatalogRef.Контрагенты</v8:Type></Type>
        </Properties>
      </Attribute>
      <Command uuid="c1">
        <Properties>
          <Name>СчетаПокупателям</Name>
          <Synonym>
            <v8:item>
              <v8:lang>ru</v8:lang>
              <v8:content>Счета покупателям (заказ)</v8:content>
            </v8:item>
          </Synonym>
          <Group>NavigationPanelSeeAlso</Group>
          <CommandParameterType/>
          <ModifiesData>false</ModifiesData>
        </Properties>
      </Command>
      <Command uuid="c2">
        <Properties>
          <Name>ОткрытьШтрихкоды</Name>
          <Synonym/>
        </Properties>
      </Command>
    </ChildObjects>
  </Document>
</MetaDataObject>"#;
        let st = parse_object_structure_xml(xml).unwrap();
        assert_eq!(
            st.commands,
            vec![
                (
                    "СчетаПокупателям".to_string(),
                    Some("Счета покупателям (заказ)".to_string())
                ),
                ("ОткрытьШтрихкоды".to_string(), None),
            ]
        );
        // Команда не попала в реквизиты, реквизит — не в команды.
        assert_eq!(st.attributes.len(), 1);
        assert_eq!(st.attributes[0].name, "Контрагент");
        // JSON-сериализация: массив объектов {name, synonym?}.
        let j = st.to_json();
        let cs = j.get("commands").unwrap().as_array().unwrap();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0]["name"], "СчетаПокупателям");
        assert_eq!(cs[0]["synonym"], "Счета покупателям (заказ)");
        assert_eq!(cs[1]["name"], "ОткрытьШтрихкоды");
        assert!(cs[1].get("synonym").is_none());
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
    fn parses_document_posting_properties() {
        // WS-1: корневые свойства проведения документа → секция "posting".
        let xml = r#"<?xml version="1.0"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Document uuid="root">
    <Properties>
      <Name>РеализацияТоваровУслуг</Name>
      <Posting>Allow</Posting>
      <RealTimePosting>Deny</RealTimePosting>
      <RegisterRecordsDeletion>AutoDeleteOff</RegisterRecordsDeletion>
      <RegisterRecordsWritingOnPost>WriteSelected</RegisterRecordsWritingOnPost>
    </Properties>
    <ChildObjects>
      <Attribute uuid="a1"><Properties><Name>Контрагент</Name>
        <Type><v8:Type>cfg:CatalogRef.Контрагенты</v8:Type></Type>
      </Properties></Attribute>
    </ChildObjects>
  </Document>
</MetaDataObject>"#;
        let st = parse_object_structure_xml(xml).unwrap();
        // 4 свойства проведения; реквизит шапки в posting не попал.
        assert_eq!(st.posting.len(), 4);
        let get = |k: &str| {
            st.posting
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("Posting"), Some("Allow"));
        assert_eq!(get("RealTimePosting"), Some("Deny"));
        assert_eq!(get("RegisterRecordsDeletion"), Some("AutoDeleteOff"));
        assert_eq!(get("RegisterRecordsWritingOnPost"), Some("WriteSelected"));
        // Реквизит Контрагент распарсился отдельно, не в posting.
        assert_eq!(st.attributes.len(), 1);
        assert_eq!(st.attributes[0].name, "Контрагент");
        // to_json: секция posting присутствует объектом {имя: значение}.
        let j = st.to_json();
        let posting = j.get("posting").unwrap().as_object().unwrap();
        assert_eq!(
            posting.get("RegisterRecordsDeletion").unwrap(),
            "AutoDeleteOff"
        );
    }

    #[test]
    fn to_json_omits_posting_for_non_document() {
        // У объекта без свойств проведения (справочник) секции posting нет.
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
        assert!(st.posting.is_empty());
        assert!(!st.to_json().as_object().unwrap().contains_key("posting"));
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
