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
    /// Тип ребра: `attr` | `tabular_attr` | `register_dim`.
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
    owner_full_name: &str,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
