//! Разбор корневых XML один раз на весь XML-слой надстройки.
//!
//! Единый обход (`scan.rs`) находит файлы, а этот модуль читает и разбирает
//! их так, чтобы каждый объектный XML был прочитан и распарсен РОВНО ОДИН раз:
//! связи данных, структура, синоним, UUID объекта и UUID его команд нужны
//! четырём разным фазам, которые раньше открывали один и тот же файл каждая
//! сама. Разбор идёт по уже прочитанному содержимому (`*_xml`-варианты
//! парсеров), поэтому дополнительных обращений к диску нет.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::xml::object_attributes::parse_object_header_xml;
use crate::xml::object_attributes::{
    parse_object_attributes_xml, parse_object_structure_content, DataLinkEdge, ObjectStructure,
};
use crate::xml::object_uuid::{
    extract_all_command_uuids_from_str, extract_form_uuid_any_from_str,
    extract_object_uuid_from_str,
};

use super::common::OBJECT_FOLDERS;
use super::RepoScan;

/// Разобранные данные одного корневого XML объекта.
pub(crate) struct ObjectXmlEntry {
    pub path: PathBuf,
    /// Имя папки типа (`Catalogs`, `Documents`, …) — по нему фазы выбирают
    /// объекты со ссылочной структурой (`OBJECT_FOLDERS`).
    pub folder: String,
    /// Имя файла без расширения — из него строится `owner_full_name` там, где
    /// раньше это делала фаза (связи данных, структура).
    pub stem: String,
    /// `(meta_type, name)` из шапки XML. `None` — шапка не разобралась; такие
    /// файлы всё равно участвуют в связях/структуре (им нужен только путь), но
    /// синоним для них не заводится — как и в прежнем проходе синонимов.
    pub header: Option<(String, String)>,
    pub synonym: Option<String>,
    /// Файл лежит в папке из [`OBJECT_FOLDERS`]: для него разобраны структура
    /// и рёбра связей данных.
    pub structured: bool,
    pub structure: Option<ObjectStructure>,
    pub edges: Vec<DataLinkEdge>,
    pub uuid: Option<String>,
    /// Имя команды объекта → её UUID (описаны внутри объектного XML).
    pub command_uuids: HashMap<String, String>,
}

/// Итог разбора корневых XML и описаний форм.
pub(crate) struct XmlHarvest {
    pub objects: Vec<ObjectXmlEntry>,
    /// `Forms/<Имя>.xml` (сосед папки формы) → UUID формы.
    pub form_descriptor_uuids: HashMap<PathBuf, String>,
}

/// Идентификаторы XML-владельцев для перечня модулей (`metadata_modules`).
///
/// Собран из [`XmlHarvest`], чтобы `build_module_row` не открывал объектный XML
/// на каждый `.bsl` (а для модулей команд — на каждую команду).
#[derive(Default)]
pub(crate) struct XmlIdCache {
    /// Корневой XML объекта (или `Form.xml`) → UUID.
    pub object_uuids: HashMap<PathBuf, String>,
    /// Корневой XML объекта → (имя команды → UUID).
    pub command_uuids: HashMap<PathBuf, HashMap<String, String>>,
    /// `Forms/<Имя>.xml` → UUID формы.
    pub form_uuids: HashMap<PathBuf, String>,
}

impl XmlIdCache {
    /// UUID владельца по пути его XML. `None` — файла в разборе не было,
    /// вызывающий читает его с диска как раньше.
    pub(crate) fn file_uuid(&self, path: &Path) -> Option<&str> {
        self.object_uuids
            .get(path)
            .or_else(|| self.form_uuids.get(path))
            .map(String::as_str)
    }

    /// UUID команды объекта по пути объектного XML и имени команды.
    pub(crate) fn command_uuid(&self, path: &Path, command: &str) -> Option<&str> {
        self.command_uuids
            .get(path)
            .and_then(|m| m.get(command))
            .map(String::as_str)
    }
}

impl XmlHarvest {
    /// Прочитать и разобрать корневые XML областей выгрузки.
    ///
    /// Ошибки чтения/разбора не фатальны: файл пропускается, как это делала
    /// каждая фаза по отдельности (раньше каждая ещё и логировала — теперь
    /// предупреждение одно на файл).
    pub(crate) fn build(scan: &RepoScan) -> Self {
        let mut objects: Vec<ObjectXmlEntry> = Vec::with_capacity(scan.root_xmls.len());
        for path in &scan.root_xmls {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let folder = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let (header, synonym) = match parse_object_header_xml(&content) {
                Some((mt, nm, syn)) => (Some((mt, nm)), syn),
                None => (None, None),
            };
            let structured = OBJECT_FOLDERS.iter().any(|(f, _)| *f == folder);

            let (structure, edges) = if structured {
                let structure = match parse_object_structure_content(path, &content) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        tracing::warn!("object_attributes: {}: {}", path.display(), e);
                        None
                    }
                };
                let edges = match parse_object_attributes_xml(&content) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!("data_links: {}: {}", path.display(), e);
                        Vec::new()
                    }
                };
                (structure, edges)
            } else {
                (None, Vec::new())
            };

            objects.push(ObjectXmlEntry {
                path: path.clone(),
                folder,
                stem,
                header,
                synonym,
                structured,
                structure,
                edges,
                uuid: extract_object_uuid_from_str(&content),
                command_uuids: extract_all_command_uuids_from_str(&content),
            });
        }

        let mut form_descriptor_uuids: HashMap<PathBuf, String> = HashMap::new();
        for path in &scan.form_descriptors {
            if let Ok(content) = std::fs::read_to_string(path) {
                if let Some(uuid) = extract_form_uuid_any_from_str(&content) {
                    if !uuid.is_empty() {
                        form_descriptor_uuids.insert(path.clone(), uuid);
                    }
                }
            }
        }

        Self {
            objects,
            form_descriptor_uuids,
        }
    }

    /// Сводный кэш идентификаторов для перечня модулей.
    pub(crate) fn id_cache(&self) -> XmlIdCache {
        let mut cache = XmlIdCache::default();
        for obj in &self.objects {
            if let Some(uuid) = &obj.uuid {
                if !uuid.is_empty() {
                    cache.object_uuids.insert(obj.path.clone(), uuid.clone());
                }
            }
            if !obj.command_uuids.is_empty() {
                cache
                    .command_uuids
                    .insert(obj.path.clone(), obj.command_uuids.clone());
            }
        }
        cache.form_uuids = self.form_descriptor_uuids.clone();
        cache
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn разбирает_объект_и_команды_за_один_проход() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        write(&repo.join("base").join("Configuration.xml"), "<x/>");
        let object = repo.join("base").join("Catalogs").join("Тест.xml");
        write(
            &object,
            r#"<MetaDataObject><Catalog uuid="cat-uuid"><Properties>
                 <Name>Тест</Name><Synonym><v8:content>Тест синоним</v8:content></Synonym>
               </Properties><ChildObjects>
                 <Attribute uuid="a1"><Properties><Name>Владелец</Name>
                   <Type><v8:Type>cfg:CatalogRef.Другой</v8:Type></Type>
                 </Properties></Attribute>
                 <Command uuid="cmd-uuid"><Properties><Name>Печать</Name></Properties></Command>
               </ChildObjects></Catalog></MetaDataObject>"#,
        );
        let form_descriptor = repo
            .join("base")
            .join("Catalogs")
            .join("Тест")
            .join("Forms")
            .join("ФормаЭлемента.xml");
        write(
            &form_descriptor,
            r#"<MetaDataObject><Form uuid="form-uuid"/></MetaDataObject>"#,
        );

        let scan = RepoScan::build(&repo);
        let harvest = XmlHarvest::build(&scan);

        assert_eq!(harvest.objects.len(), 1);
        let obj = &harvest.objects[0];
        assert_eq!(obj.stem, "Тест");
        assert_eq!(
            obj.header
                .as_ref()
                .map(|(mt, nm)| (mt.as_str(), nm.as_str())),
            Some(("Catalog", "Тест"))
        );
        assert_eq!(obj.synonym.as_deref(), Some("Тест синоним"));
        assert!(obj.structured, "Catalogs — папка OBJECT_FOLDERS");
        assert_eq!(obj.edges.len(), 1, "ссылочный реквизит даёт ребро");
        assert!(obj.structure.is_some());
        assert_eq!(obj.uuid.as_deref(), Some("cat-uuid"));
        assert_eq!(
            obj.command_uuids.get("Печать").map(String::as_str),
            Some("cmd-uuid")
        );

        let ids = harvest.id_cache();
        assert_eq!(ids.file_uuid(&object), Some("cat-uuid"));
        assert_eq!(ids.command_uuid(&object, "Печать"), Some("cmd-uuid"));
        assert_eq!(ids.file_uuid(&form_descriptor), Some("form-uuid"));
    }

    /// Объект без разобравшейся шапки всё равно попадает в разбор: связям и
    /// структуре нужен путь, а не имя. Синоним для него не заводится.
    #[test]
    fn объект_без_шапки_остаётся_для_связей() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        write(&repo.join("base").join("Configuration.xml"), "<x/>");
        let object = repo.join("base").join("Documents").join("БезИмени.xml");
        write(
            &object,
            r#"<MetaDataObject><Document uuid="d-uuid"><ChildObjects>
                 <Attribute uuid="a1"><Properties><Name>Ссылка</Name>
                   <Type><v8:Type>cfg:CatalogRef.Тест</v8:Type></Type>
                 </Properties></Attribute>
               </ChildObjects></Document></MetaDataObject>"#,
        );

        let scan = RepoScan::build(&repo);
        let harvest = XmlHarvest::build(&scan);
        assert_eq!(harvest.objects.len(), 1);
        assert!(harvest.objects[0].header.is_none());
        assert_eq!(harvest.objects[0].edges.len(), 1);
        assert_eq!(harvest.objects[0].uuid.as_deref(), Some("d-uuid"));
    }
}
