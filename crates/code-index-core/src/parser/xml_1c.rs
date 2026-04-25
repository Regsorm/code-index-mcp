/// Парсер XML-выгрузок 1С:Предприятие.
/// Использует событийный (SAX-подобный) парсинг через quick-xml
/// без tree-sitter — XML-грамматика не нужна, структура предсказуема.
use anyhow::Result;
use quick_xml::Reader;
use quick_xml::events::Event;

use super::types::{sha256_hex, ParseResult, ParsedClass, ParsedVariable};
use super::LanguageParser;

/// Парсер XML-файлов выгрузок конфигурации 1С
pub struct Xml1CParser;

/// Известные типы объектов метаданных 1С
const METADATA_TYPES: &[&str] = &[
    "Catalog",
    "Document",
    "InformationRegister",
    "AccumulationRegister",
    "DataProcessor",
    "Report",
    "CommonModule",
    "ChartOfCharacteristicTypes",
    "ExchangePlan",
    "BusinessProcess",
    "Task",
    "ChartOfAccounts",
    "AccountingRegister",
    "CalculationRegister",
    "ChartOfCalculationTypes",
    "Enum",
    "Constant",
    "Subsystem",
];

impl Xml1CParser {
    pub fn new() -> Self {
        Xml1CParser
    }
}

impl LanguageParser for Xml1CParser {
    fn language_name(&self) -> &str {
        "xml_1c"
    }

    fn file_extensions(&self) -> &[&str] {
        // Не регистрируем в ParserRegistry — вызов только через indexer напрямую
        &["xml"]
    }

    fn parse(&self, source: &str, file_path: &str) -> Result<ParseResult> {
        parse_xml_1c(source, file_path)
    }
}

/// Проверить, является ли содержимое файла выгрузкой 1С.
/// Ищем тег <MetaDataObject в первых 500 байтах — быстрая проверка без полного парсинга.
fn is_1c_xml(source: &str) -> bool {
    // Безопасная обрезка по границе символов UTF-8
    let end = source.len().min(500);
    let safe_end = source.floor_char_boundary(end);
    let probe = &source[..safe_end];
    probe.contains("<MetaDataObject")
}

/// Контекст парсинга — отслеживает текущее положение в дереве XML
#[derive(Debug, Default)]
struct ParseContext {
    /// Стек тегов (имена без namespace-префикса)
    tag_stack: Vec<String>,
    /// Тип объекта метаданных ("Catalog", "Document", и т.д.)
    object_type: Option<String>,
    /// Имя объекта метаданных (из <Name> внутри корневых <Properties>)
    object_name: Option<String>,
    /// Синоним объекта (из <v8:content> внутри <Synonym>)
    object_synonym: Option<String>,
    /// Флаг: мы внутри тега <Properties>
    in_properties: bool,
    /// Глубина вложенности Properties (Properties могут быть вложены)
    properties_depth: usize,
    /// Флаг: мы внутри тега <Attribute>
    in_attribute: bool,
    /// Глубина вложенности Attribute
    attribute_depth: usize,
    /// Флаг: мы внутри тега <TabularSection>
    in_tabular_section: bool,
    /// Имя текущей табличной части
    tabular_section_name: Option<String>,
    /// Флаг: мы внутри тега <Form>
    in_form: bool,
    /// Имя текущей формы
    form_name: Option<String>,
    /// Флаг: мы внутри <Synonym>
    in_synonym: bool,
    /// Флаг: мы ищем content синонима (следующий <v8:content>)
    reading_synonym: bool,
    /// Имя текущего реквизита (пока собирается)
    current_attribute_name: Option<String>,
    /// Флаг: корневые <Properties> объекта ещё не прочитаны
    root_properties_done: bool,
}

impl ParseContext {
    /// Проверить: находимся ли в цепочке тегов Properties > Name
    fn in_properties_name(&self) -> bool {
        if self.tag_stack.len() < 2 {
            return false;
        }
        let len = self.tag_stack.len();
        self.tag_stack[len - 1] == "Name" && self.tag_stack[len - 2] == "Properties"
    }
}

/// Нормализовать имя тега: убрать namespace-префикс (например, "v8:item" → "item")
fn strip_ns(tag: &str) -> &str {
    if let Some(pos) = tag.rfind(':') {
        &tag[pos + 1..]
    } else {
        tag
    }
}

/// Основная функция парсинга XML-выгрузки 1С
fn parse_xml_1c(source: &str, _file_path: &str) -> Result<ParseResult> {
    // Быстрая предварительная проверка
    if !is_1c_xml(source) {
        return Ok(ParseResult {
            functions: vec![],
            classes: vec![],
            imports: vec![],
            calls: vec![],
            variables: vec![],
            lines_total: source.lines().count(),
            ast_hash: String::new(),
        });
    }

    let mut classes: Vec<ParsedClass> = vec![];
    let mut variables: Vec<ParsedVariable> = vec![];
    let lines_total = source.lines().count();

    let mut reader = Reader::from_str(source);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    let mut ctx = ParseContext::default();
    let mut current_line: usize = 1;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                // Получаем имя тега без namespace
                let raw = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let tag = strip_ns(&raw).to_string();
                ctx.tag_stack.push(tag.clone());

                match tag.as_str() {
                    t if METADATA_TYPES.contains(&t) => {
                        // Встретили тип объекта метаданных
                        if ctx.object_type.is_none() {
                            ctx.object_type = Some(t.to_string());
                        }
                    }
                    "Properties" => {
                        ctx.in_properties = true;
                        ctx.properties_depth += 1;
                    }
                    "Attribute" => {
                        ctx.in_attribute = true;
                        ctx.attribute_depth += 1;
                        // Сбрасываем имя реквизита при входе
                        if ctx.attribute_depth == 1 {
                            ctx.current_attribute_name = None;
                        }
                    }
                    "TabularSection" => {
                        if !ctx.in_tabular_section {
                            ctx.in_tabular_section = true;
                            ctx.tabular_section_name = None;
                        }
                    }
                    "Form" => {
                        ctx.in_form = true;
                        ctx.form_name = None;
                    }
                    "Synonym" => {
                        ctx.in_synonym = true;
                    }
                    "content" => {
                        // <v8:content> внутри Synonym — читаем синоним объекта
                        if ctx.in_synonym && !ctx.root_properties_done {
                            ctx.reading_synonym = true;
                        }
                    }
                    _ => {}
                }
            }

            Ok(Event::End(ref e)) => {
                let raw = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let tag = strip_ns(&raw).to_string();

                match tag.as_str() {
                    "Properties" => {
                        if ctx.properties_depth > 0 {
                            ctx.properties_depth -= 1;
                        }
                        if ctx.properties_depth == 0 {
                            ctx.in_properties = false;
                            // Если объект не имеет имени ещё — пометим корневые Properties прочитанными
                            if ctx.object_name.is_some() {
                                ctx.root_properties_done = true;
                            }
                        }
                    }
                    "Attribute" => {
                        if ctx.attribute_depth > 0 {
                            ctx.attribute_depth -= 1;
                        }
                        if ctx.attribute_depth == 0 {
                            // Сохраняем реквизит
                            if let Some(attr_name) = ctx.current_attribute_name.take() {
                                let parent = if let Some(ref ts) = ctx.tabular_section_name {
                                    Some(ts.clone())
                                } else {
                                    ctx.object_name.clone()
                                };
                                variables.push(ParsedVariable {
                                    name: attr_name,
                                    value: parent,
                                    line: current_line,
                                });
                            }
                            ctx.in_attribute = false;
                        }
                    }
                    "TabularSection" => {
                        ctx.in_tabular_section = false;
                        ctx.tabular_section_name = None;
                    }
                    "Form" => {
                        // Сохраняем форму как переменную
                        if let Some(form_name) = ctx.form_name.take() {
                            variables.push(ParsedVariable {
                                name: format!("Форма.{}", form_name),
                                value: ctx.object_name.clone(),
                                line: current_line,
                            });
                        }
                        ctx.in_form = false;
                    }
                    "Synonym" => {
                        ctx.in_synonym = false;
                        ctx.reading_synonym = false;
                    }
                    "content" => {
                        ctx.reading_synonym = false;
                    }
                    _ => {}
                }

                ctx.tag_stack.pop();
            }

            Ok(Event::Text(ref e)) => {
                let text = match e.unescape() {
                    Ok(t) => t.to_string(),
                    Err(_) => {
                        buf.clear();
                        continue;
                    }
                };
                let text = text.trim().to_string();
                if text.is_empty() {
                    buf.clear();
                    continue;
                }

                // <v8:content> внутри Synonym — синоним объекта
                if ctx.reading_synonym && ctx.object_synonym.is_none() {
                    ctx.object_synonym = Some(text.clone());
                }
                // <Name> внутри <Properties>
                else if ctx.in_properties_name() {
                    if ctx.in_attribute && ctx.attribute_depth > 0 {
                        // Имя реквизита (самого верхнего уровня — depth == 1)
                        if ctx.attribute_depth == 1 && ctx.current_attribute_name.is_none() {
                            ctx.current_attribute_name = Some(text.clone());
                        }
                    } else if ctx.in_tabular_section && ctx.tabular_section_name.is_none() {
                        // Имя табличной части
                        let ts_name = text.clone();
                        // Сохраняем табличную часть как класс
                        classes.push(ParsedClass {
                            name: format!("ТабличнаяЧасть.{}", ts_name),
                            line_start: current_line,
                            line_end: current_line,
                            bases: Some("TabularSection".to_string()),
                            docstring: None,
                            body: String::new(),
                            node_hash: sha256_hex(&format!("tabular_section:{}", ts_name)),
                        });
                        ctx.tabular_section_name = Some(ts_name);
                    } else if ctx.in_form && ctx.form_name.is_none() {
                        // Имя формы
                        ctx.form_name = Some(text.clone());
                    } else if ctx.object_type.is_some()
                        && ctx.object_name.is_none()
                        && !ctx.in_attribute
                        && !ctx.in_tabular_section
                    {
                        // Имя корневого объекта метаданных
                        ctx.object_name = Some(text.clone());
                    }
                }
            }

            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }

        // Обновляем примерный номер строки через позицию ридера
        current_line = reader.buffer_position() as usize;
        buf.clear();
    }

    // Если нашли объект — добавляем его как класс
    if let Some(obj_type) = ctx.object_type {
        if let Some(obj_name) = ctx.object_name {
            let node_hash = sha256_hex(&format!("{}:{}", obj_type, obj_name));
            classes.insert(
                0,
                ParsedClass {
                    name: obj_name.clone(),
                    line_start: 1,
                    line_end: lines_total,
                    bases: Some(obj_type),
                    docstring: ctx.object_synonym,
                    body: String::new(),
                    node_hash: node_hash.clone(),
                },
            );

            // ast_hash — от имени объекта + количество реквизитов
            let ast_hash = sha256_hex(&format!(
                "{}:{}:{}",
                obj_name,
                classes.len(),
                variables.len()
            ));

            return Ok(ParseResult {
                functions: vec![],
                classes,
                imports: vec![],
                calls: vec![],
                variables,
                lines_total,
                ast_hash,
            });
        }
    }

    // Тег MetaDataObject есть, но структура не распознана — возвращаем пустой результат
    let fallback_hash = if !classes.is_empty() || !variables.is_empty() {
        sha256_hex(source)
    } else {
        String::new()
    };
    Ok(ParseResult {
        functions: vec![],
        classes,
        imports: vec![],
        calls: vec![],
        variables,
        lines_total,
        ast_hash: fallback_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::LanguageParser;

    #[test]
    fn test_parse_catalog_xml() {
        let source = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core">
    <Catalog uuid="123">
        <Properties>
            <Name>Контрагенты</Name>
            <Synonym>
                <v8:item>
                    <v8:lang>ru</v8:lang>
                    <v8:content>Контрагенты</v8:content>
                </v8:item>
            </Synonym>
        </Properties>
        <ChildObjects>
            <Attribute uuid="a1">
                <Properties>
                    <Name>ИНН</Name>
                </Properties>
            </Attribute>
            <Attribute uuid="a2">
                <Properties>
                    <Name>КПП</Name>
                </Properties>
            </Attribute>
            <TabularSection uuid="t1">
                <Properties>
                    <Name>КонтактнаяИнформация</Name>
                </Properties>
                <ChildObjects>
                    <Attribute uuid="ta1">
                        <Properties>
                            <Name>Тип</Name>
                        </Properties>
                    </Attribute>
                </ChildObjects>
            </TabularSection>
        </ChildObjects>
    </Catalog>
</MetaDataObject>"#;
        let parser = Xml1CParser;
        let result = parser.parse(source, "Catalogs/Контрагенты.xml").unwrap();

        // Объект метаданных
        assert!(
            result.classes.iter().any(|c| c.name == "Контрагенты"
                && c.bases == Some("Catalog".to_string())),
            "Должен быть класс Контрагенты с базой Catalog, найдено: {:?}",
            result.classes
        );

        // Реквизиты
        assert!(
            result.variables.iter().any(|v| v.name == "ИНН"),
            "Должен быть реквизит ИНН, найдено: {:?}",
            result.variables
        );
        assert!(
            result.variables.iter().any(|v| v.name == "КПП"),
            "Должен быть реквизит КПП, найдено: {:?}",
            result.variables
        );

        // Табличная часть
        assert!(
            result.classes.iter().any(|c| c.name.contains("КонтактнаяИнформация")),
            "Должна быть табличная часть КонтактнаяИнформация, найдено: {:?}",
            result.classes
        );
    }

    #[test]
    fn test_non_1c_xml_returns_empty() {
        let source = r#"<?xml version="1.0"?><root><item>test</item></root>"#;
        let parser = Xml1CParser;
        let result = parser.parse(source, "test.xml").unwrap();
        assert!(result.classes.is_empty(), "Обычный XML должен дать пустые классы");
        assert!(result.variables.is_empty(), "Обычный XML должен дать пустые переменные");
    }

    #[test]
    fn test_parse_document_xml() {
        let source = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
    <Document uuid="d1">
        <Properties>
            <Name>РеализацияТоваров</Name>
        </Properties>
        <ChildObjects>
            <Attribute uuid="a1">
                <Properties><Name>Контрагент</Name></Properties>
            </Attribute>
        </ChildObjects>
    </Document>
</MetaDataObject>"#;
        let parser = Xml1CParser;
        let result = parser.parse(source, "Documents/РеализацияТоваров.xml").unwrap();
        assert!(
            result.classes.iter().any(|c| c.name == "РеализацияТоваров"
                && c.bases == Some("Document".to_string())),
            "Должен быть класс РеализацияТоваров с базой Document"
        );
        assert!(
            result.variables.iter().any(|v| v.name == "Контрагент"),
            "Должен быть реквизит Контрагент"
        );
    }

    #[test]
    fn test_parse_information_register() {
        let source = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
    <InformationRegister uuid="ir1">
        <Properties>
            <Name>КурсыВалют</Name>
        </Properties>
        <ChildObjects>
            <Attribute uuid="a1">
                <Properties><Name>Курс</Name></Properties>
            </Attribute>
            <Attribute uuid="a2">
                <Properties><Name>Кратность</Name></Properties>
            </Attribute>
        </ChildObjects>
    </InformationRegister>
</MetaDataObject>"#;
        let parser = Xml1CParser;
        let result = parser.parse(source, "InformationRegisters/КурсыВалют.xml").unwrap();
        assert!(
            result.classes.iter().any(|c| c.name == "КурсыВалют"
                && c.bases == Some("InformationRegister".to_string())),
            "Должен быть класс КурсыВалют с базой InformationRegister"
        );
        assert!(result.variables.iter().any(|v| v.name == "Курс"));
        assert!(result.variables.iter().any(|v| v.name == "Кратность"));
    }

    #[test]
    fn test_parse_common_module() {
        let source = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
    <CommonModule uuid="cm1">
        <Properties>
            <Name>ОбщегоНазначения</Name>
        </Properties>
    </CommonModule>
</MetaDataObject>"#;
        let parser = Xml1CParser;
        let result = parser.parse(source, "CommonModules/ОбщегоНазначения.xml").unwrap();
        assert!(
            result.classes.iter().any(|c| c.name == "ОбщегоНазначения"
                && c.bases == Some("CommonModule".to_string())),
            "Должен быть класс ОбщегоНазначения с базой CommonModule"
        );
    }

    #[test]
    fn test_tabular_section_attributes() {
        // Реквизиты внутри табличной части должны попасть в variables
        let source = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
    <Document uuid="d1">
        <Properties>
            <Name>ПоступлениеТоваров</Name>
        </Properties>
        <ChildObjects>
            <TabularSection uuid="ts1">
                <Properties>
                    <Name>Товары</Name>
                </Properties>
                <ChildObjects>
                    <Attribute uuid="ta1">
                        <Properties><Name>Номенклатура</Name></Properties>
                    </Attribute>
                    <Attribute uuid="ta2">
                        <Properties><Name>Количество</Name></Properties>
                    </Attribute>
                </ChildObjects>
            </TabularSection>
        </ChildObjects>
    </Document>
</MetaDataObject>"#;
        let parser = Xml1CParser;
        let result = parser.parse(source, "Documents/ПоступлениеТоваров.xml").unwrap();

        // Документ
        assert!(result.classes.iter().any(|c| c.name == "ПоступлениеТоваров"));
        // Табличная часть
        assert!(result.classes.iter().any(|c| c.name.contains("Товары")));
        // Реквизиты табличной части
        assert!(result.variables.iter().any(|v| v.name == "Номенклатура"));
        assert!(result.variables.iter().any(|v| v.name == "Количество"));
    }
}
