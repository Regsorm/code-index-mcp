//! Единый обход дерева репозитория для XML-слоя надстройки.
//!
//! До этого модуля каждая фаза слоя ходила по дереву сама: `sub_config_roots`
//! вызывался восемь раз, `index_metadata_forms` / `index_object_templates` /
//! `index_metadata_modules` обходили весь репозиторий целиком, остальные — свои
//! подкаталоги. На 90 тыс. файлов это больше десяти полных обходов, каждый со
//! `stat`-ом на Windows. Здесь дерево обходится ОДИН раз, а фазы получают
//! готовые списки файлов по категориям.
//!
//! Набор категорий и правила классификации повторяют ровно те выборки, что
//! делали фазы своими обходами (см. `full_scan.rs` / `modules.rs`): множества
//! файлов совпадают, поэтому результаты записи в БД не меняются.
//!
//! Обход конфигураций (`Configuration.xml`, глубина ≤ 3) вынесен в отдельный
//! проход с `max_depth(3)`: так сохраняется прежний ПОРЯДОК находок, от
//! которого зависит, чья запись побеждает в `INSERT OR IGNORE` при
//! заимствованных объектах (`base/` должен идти раньше `extensions/`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use super::common::DirFilter;

/// Результат одного обхода: пути файлов, разложенные по категориям.
pub(crate) struct RepoScan {
    pub(crate) repo_root: PathBuf,
    /// `Configuration.xml` на глубине ≤ 3, в порядке обхода.
    pub(crate) config_paths: Vec<PathBuf>,
    /// Корни sub-config (родители `config_paths`), base-first.
    pub(crate) sub_roots: Vec<PathBuf>,
    /// Множество `sub_roots` для быстрой проверки принадлежности.
    sub_root_set: HashSet<PathBuf>,
    /// XML, лежащий ПРЯМО в дочерней папке sub-root — корневые описания
    /// объектов (`<sub>/<ПапкаТипа>/<Имя>.xml`). Сюда же попадают
    /// `<sub>/Subsystems/<Имя>.xml` и `<sub>/DefinedTypes/<Имя>.xml` — их
    /// читает и проход синонимов, как и раньше.
    pub(crate) root_xmls: Vec<PathBuf>,
    /// XML прямо внутри каталога `Subsystems` (верхние и вложенные).
    pub(crate) nested_subsystems: Vec<PathBuf>,
    /// `<sub>/ExchangePlans/<Имя>/Ext/Content.xml`.
    pub(crate) exchange_content: Vec<PathBuf>,
    /// `<sub>/DefinedTypes/<Имя>.xml`.
    pub(crate) defined_types: Vec<PathBuf>,
    /// `<sub>/FunctionalOptions/<Имя>.xml`.
    pub(crate) functional_options: Vec<PathBuf>,
    /// `Roles/**/Rights.xml`.
    pub(crate) rights_files: Vec<PathBuf>,
    /// Файлы с именем `Form.xml`.
    pub(crate) form_xmls: Vec<PathBuf>,
    /// Описания форм `<...>/Forms/<Имя>.xml` (сосед папки формы) — источник
    /// UUID формы для перечня модулей, когда `Form.xml` лежит глубже.
    pub(crate) form_descriptors: Vec<PathBuf>,
    /// `<...>/EventSubscriptions/<Имя>.xml` (глубина ≤ 4, как в прежнем обходе).
    pub(crate) event_sub_xmls: Vec<PathBuf>,
    /// Описания макетов `<...>/Templates/<Имя>.xml`.
    pub(crate) template_descriptors: Vec<PathBuf>,
    /// Все `.bsl` файлы репозитория.
    pub(crate) bsl_files: Vec<PathBuf>,
    /// Содержимое макетов (`<...>/Ext/Template.*`) и предопределённые элементы
    /// (`<Объект>/Ext/Predefined.xml`) — вход фаз надстройки, который не попадает
    /// в XML/BSL-категории выше (например, `Template.dcs`), но обязан входить в
    /// отпечаток возобновления: иначе правка только содержимого макета или
    /// предопределённых не сдвинула бы отпечаток и фаза была бы пропущена.
    pub(crate) content_files: Vec<PathBuf>,
    /// Все `ConfigDumpInfo.xml` (по одному на область выгрузки).
    pub(crate) dump_info_files: Vec<PathBuf>,
}

impl RepoScan {
    /// Обойти репозиторий один раз и разложить найденные файлы по категориям.
    pub(crate) fn build(repo_root: &Path) -> Self {
        let filter = DirFilter::load(repo_root);

        // 1. Конфигурации — отдельным проходом с прежней глубиной: порядок
        //    находок определяет приоритет при заимствовании объектов.
        let mut config_paths: Vec<PathBuf> = Vec::new();
        for entry in WalkDir::new(repo_root)
            .max_depth(3)
            .into_iter()
            .filter_entry(|e| filter.allows(e))
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file()
                && entry.file_name().to_str() == Some("Configuration.xml")
            {
                config_paths.push(entry.path().to_path_buf());
            }
        }

        // 2. Корни sub-config, base-first (как `sub_config_roots`).
        let mut sub_roots: Vec<PathBuf> = config_paths
            .iter()
            .filter_map(|p| p.parent().map(Path::to_path_buf))
            .collect();
        sub_roots.sort_by_key(|p| u8::from(p.components().any(|c| c.as_os_str() == "extensions")));
        let sub_root_set: HashSet<PathBuf> = sub_roots.iter().cloned().collect();

        // 3. Один полный обход: собираем только нужные расширения.
        let mut xml_all: Vec<PathBuf> = Vec::new();
        let mut bsl_files: Vec<PathBuf> = Vec::new();
        let mut content_files: Vec<PathBuf> = Vec::new();
        for entry in WalkDir::new(repo_root)
            .into_iter()
            .filter_entry(|e| filter.allows(e))
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            match path.extension().and_then(|e| e.to_str()) {
                Some(ext) if ext.eq_ignore_ascii_case("xml") => xml_all.push(path.to_path_buf()),
                Some(ext) if ext.eq_ignore_ascii_case("bsl") => bsl_files.push(path.to_path_buf()),
                _ => {}
            }
            let name = entry.file_name().to_str().unwrap_or("");
            let parent_is_ext = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                == Some("Ext");
            if parent_is_ext && (name == "Predefined.xml" || name.starts_with("Template.")) {
                content_files.push(path.to_path_buf());
            }
        }

        // 4. Классификация. Файл может попадать в НЕСКОЛЬКО категорий: корневой
        //    XML типа одновременно читают связи данных, структура, синонимы и
        //    UUID модулей — категории не взаимоисключающие.
        let mut scan = Self {
            repo_root: repo_root.to_path_buf(),
            config_paths,
            sub_roots,
            sub_root_set,
            root_xmls: Vec::new(),
            nested_subsystems: Vec::new(),
            exchange_content: Vec::new(),
            defined_types: Vec::new(),
            functional_options: Vec::new(),
            rights_files: Vec::new(),
            form_xmls: Vec::new(),
            form_descriptors: Vec::new(),
            event_sub_xmls: Vec::new(),
            template_descriptors: Vec::new(),
            bsl_files,
            content_files,
            dump_info_files: Vec::new(),
        };

        for path in &xml_all {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "Configuration.xml" {
                continue;
            }
            if name == "ConfigDumpInfo.xml" {
                scan.dump_info_files.push(path.clone());
                continue;
            }
            let parent = path.parent();
            let parent_name = parent
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("");
            // «Корневой» XML типа: файл лежит прямо в дочерней папке sub-root.
            let in_root_xml = parent
                .and_then(|p| p.parent())
                .is_some_and(|gp| scan.sub_root_set.contains(gp));
            if in_root_xml {
                scan.root_xmls.push(path.clone());
            }

            match name {
                "Form.xml" => scan.form_xmls.push(path.clone()),
                "Rights.xml" => scan.rights_files.push(path.clone()),
                "Content.xml" => {
                    if is_exchange_content(path, &scan.sub_root_set) {
                        scan.exchange_content.push(path.clone());
                    }
                }
                _ => {}
            }

            match parent_name {
                "Subsystems" => scan.nested_subsystems.push(path.clone()),
                "Forms" => scan.form_descriptors.push(path.clone()),
                "Templates" => scan.template_descriptors.push(path.clone()),
                "EventSubscriptions" => {
                    if rel_depth(repo_root, path) <= 4 {
                        scan.event_sub_xmls.push(path.clone());
                    }
                }
                "DefinedTypes" if in_root_xml => scan.defined_types.push(path.clone()),
                "FunctionalOptions" if in_root_xml => scan.functional_options.push(path.clone()),
                _ => {}
            }
        }

        scan
    }

    /// Ближайший к файлу корень sub-config (самый длинный из префиксов).
    /// `None` — файл лежит вне областей выгрузки (например, внешние обработки
    /// рядом с репозиторием).
    pub(crate) fn sub_root_of<'a>(&'a self, path: &'a Path) -> Option<&'a Path> {
        let mut dir = path.parent();
        while let Some(d) = dir {
            if self.sub_root_set.contains(d) {
                return Some(d);
            }
            if d == self.repo_root {
                break;
            }
            dir = d.parent();
        }
        None
    }

    /// Файлы списка, лежащие в поддереве `sub_root` (в порядке исходного списка).
    pub(crate) fn files_of<'a>(
        &'a self,
        list: &'a [PathBuf],
        sub_root: &'a Path,
    ) -> impl Iterator<Item = &'a PathBuf> + 'a {
        list.iter()
            .filter(move |p| self.sub_root_of(p) == Some(sub_root))
    }
}

/// `.../ExchangePlans/<Имя>/Ext/Content.xml` внутри области выгрузки.
fn is_exchange_content(path: &Path, sub_roots: &HashSet<PathBuf>) -> bool {
    let ext_dir = match path.parent() {
        Some(p) => p,
        None => return false,
    };
    if ext_dir.file_name().and_then(|s| s.to_str()) != Some("Ext") {
        return false;
    }
    let ep_dir = match ext_dir.parent().and_then(|p| p.parent()) {
        Some(p) => p,
        None => return false,
    };
    if ep_dir.file_name().and_then(|s| s.to_str()) != Some("ExchangePlans") {
        return false;
    }
    ep_dir.parent().is_some_and(|sub| sub_roots.contains(sub))
}

/// Глубина пути относительно корня репозитория (в компонентах).
fn rel_depth(repo_root: &Path, path: &Path) -> usize {
    path.strip_prefix(repo_root)
        .map(|r| r.components().count())
        .unwrap_or(usize::MAX)
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
    fn классифицирует_категории_выгрузки() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        write(&repo.join("base").join("Configuration.xml"), "<x/>");
        write(&repo.join("base").join("ConfigDumpInfo.xml"), "<x/>");
        write(
            &repo.join("base").join("Catalogs").join("Товары.xml"),
            "<x/>",
        );
        write(
            &repo
                .join("base")
                .join("Catalogs")
                .join("Товары")
                .join("Forms")
                .join("Форма")
                .join("Ext")
                .join("Form.xml"),
            "<x/>",
        );
        write(
            &repo
                .join("base")
                .join("Catalogs")
                .join("Товары")
                .join("Forms")
                .join("Форма.xml"),
            "<x/>",
        );
        write(
            &repo
                .join("base")
                .join("Catalogs")
                .join("Товары")
                .join("Templates")
                .join("Печать.xml"),
            "<x/>",
        );
        write(
            &repo
                .join("base")
                .join("Catalogs")
                .join("Товары")
                .join("Templates")
                .join("Печать")
                .join("Ext")
                .join("Template.dcs"),
            "<x/>",
        );
        write(
            &repo
                .join("base")
                .join("Catalogs")
                .join("Товары")
                .join("Ext")
                .join("Predefined.xml"),
            "<x/>",
        );
        write(
            &repo
                .join("base")
                .join("Subsystems")
                .join("Продажи")
                .join("Subsystems")
                .join("Опт.xml"),
            "<x/>",
        );
        write(
            &repo
                .join("base")
                .join("EventSubscriptions")
                .join("Подписка.xml"),
            "<x/>",
        );
        write(
            &repo
                .join("base")
                .join("Roles")
                .join("Админ")
                .join("Ext")
                .join("Rights.xml"),
            "<x/>",
        );
        write(
            &repo.join("base").join("DefinedTypes").join("Тип.xml"),
            "<x/>",
        );
        write(
            &repo
                .join("base")
                .join("FunctionalOptions")
                .join("Опция.xml"),
            "<x/>",
        );
        write(
            &repo
                .join("base")
                .join("ExchangePlans")
                .join("Обмен")
                .join("Ext")
                .join("Content.xml"),
            "<x/>",
        );
        write(
            &repo
                .join("base")
                .join("CommonModules")
                .join("Общий")
                .join("Ext")
                .join("Module.bsl"),
            "",
        );

        let scan = RepoScan::build(&repo);

        assert_eq!(scan.sub_roots.len(), 1);
        assert_eq!(scan.config_paths.len(), 1);
        assert_eq!(
            scan.root_xmls.len(),
            4,
            "Товары, Тип, Опция, Подписка (EventSubscriptions — тоже папка типа)"
        );
        assert_eq!(scan.nested_subsystems.len(), 1, "Опт");
        assert_eq!(scan.form_xmls.len(), 1);
        assert_eq!(scan.form_descriptors.len(), 1);
        assert_eq!(scan.template_descriptors.len(), 1);
        assert_eq!(scan.event_sub_xmls.len(), 1);
        assert_eq!(scan.rights_files.len(), 1);
        assert_eq!(scan.defined_types.len(), 1);
        assert_eq!(scan.functional_options.len(), 1);
        assert_eq!(scan.exchange_content.len(), 1);
        assert_eq!(scan.bsl_files.len(), 1);
        assert_eq!(scan.dump_info_files.len(), 1);
        assert_eq!(
            scan.content_files.len(),
            2,
            "содержимое макета (Template.dcs) и предопределённые — вход фаз, \
             не попавший в XML/BSL-категории"
        );
    }

    #[test]
    fn base_идёт_раньше_расширений() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        write(
            &repo
                .join("extensions")
                .join("EF_A")
                .join("Configuration.xml"),
            "<x/>",
        );
        write(&repo.join("base").join("Configuration.xml"), "<x/>");

        let scan = RepoScan::build(&repo);
        assert_eq!(scan.sub_roots.len(), 2);
        assert!(
            scan.sub_roots[0].ends_with("base"),
            "base должен идти первым: {:?}",
            scan.sub_roots
        );
    }
}
