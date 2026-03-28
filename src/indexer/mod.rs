/// Модуль индексатора — обход директорий, определение типов файлов, хеширование
pub mod file_types;
pub mod hasher;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use walkdir::WalkDir;

use crate::parser;
use crate::parser::text::TextParser;
use crate::storage::models::*;
use crate::storage::Storage;
use file_types::{categorize_file, is_excluded_dir, FileCategory};

/// Результат одного прохода индексации
#[derive(Debug)]
pub struct IndexResult {
    /// Сколько файлов просмотрено (не считая бинарных)
    pub files_scanned: usize,
    /// Сколько файлов реально записано в БД (новые или изменённые)
    pub files_indexed: usize,
    /// Сколько файлов пропущено (хеш не изменился)
    pub files_skipped: usize,
    /// Сколько файлов удалено из БД (больше не существуют на диске)
    pub files_deleted: usize,
    /// Список ошибок: (путь, сообщение)
    pub errors: Vec<(String, String)>,
    /// Время работы в миллисекундах
    pub elapsed_ms: u64,
}

/// Индексатор файловой системы
pub struct Indexer<'a> {
    storage: &'a mut Storage,
}

impl<'a> Indexer<'a> {
    /// Создать индексатор с уже открытым хранилищем
    pub fn new(storage: &'a mut Storage) -> Self {
        Self { storage }
    }

    /// Полная переиндексация директории `root`.
    ///
    /// Если `force = true` — перезаписать все файлы независимо от хеша.
    /// Если `force = false` — пропустить файлы с неизменённым content_hash.
    ///
    /// По завершении удаляет из БД записи файлов, которых больше нет на диске.
    pub fn full_reindex(&mut self, root: &Path, force: bool) -> Result<IndexResult> {
        let start = std::time::Instant::now();
        let mut result = IndexResult {
            files_scanned: 0,
            files_indexed: 0,
            files_skipped: 0,
            files_deleted: 0,
            errors: vec![],
            elapsed_ms: 0,
        };

        // Загружаем текущее состояние БД: path -> (id, content_hash)
        let existing_files: HashMap<String, (i64, String)> = self
            .storage
            .get_all_files()?
            .into_iter()
            .filter_map(|f| {
                f.id.map(|id| (f.path.clone(), (id, f.content_hash.clone())))
            })
            .collect();

        // Набор путей, реально встреченных при обходе диска
        let mut seen_paths: HashSet<String> = HashSet::new();

        // Обходим директорию, исключая нежелательные папки
        let walker = WalkDir::new(root).into_iter().filter_entry(|e| {
            if e.file_type().is_dir() {
                if let Some(name) = e.file_name().to_str() {
                    return !is_excluded_dir(name);
                }
            }
            true
        });

        for entry in walker.filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();
            let category = categorize_file(path);

            // Бинарные файлы полностью игнорируем
            if matches!(category, FileCategory::Binary) {
                continue;
            }

            result.files_scanned += 1;

            // Нормализуем путь относительно корня с прямыми слэшами
            let rel_path = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");

            seen_paths.insert(rel_path.clone());

            // Читаем файл и вычисляем хеш
            let (content, hash) = match hasher::file_hash(path) {
                Ok(r) => r,
                Err(e) => {
                    result.errors.push((rel_path, e.to_string()));
                    continue;
                }
            };

            // Проверяем, изменился ли файл
            if !force {
                if let Some((_, existing_hash)) = existing_files.get(&rel_path) {
                    if *existing_hash == hash {
                        result.files_skipped += 1;
                        continue;
                    }
                }
            }

            // Индексируем файл
            match self.index_single_file(&rel_path, &content, &hash, &category) {
                Ok(_) => result.files_indexed += 1,
                Err(e) => result.errors.push((rel_path, e.to_string())),
            }
        }

        // Удаляем из БД файлы, которых больше нет на диске
        for (path, (id, _)) in &existing_files {
            if !seen_paths.contains(path) {
                self.storage.delete_file(*id)?;
                result.files_deleted += 1;
            }
        }

        result.elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(result)
    }

    /// Индексировать один файл: сохранить в БД метаданные и извлечённые символы.
    fn index_single_file(
        &self,
        rel_path: &str,
        content: &str,
        content_hash: &str,
        category: &FileCategory,
    ) -> Result<()> {
        match category {
            FileCategory::Code(language) => {
                // Определяем парсер по расширению файла
                let ext = Path::new(rel_path)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();

                let parser = parser::get_parser_for_extension(&ext)
                    .ok_or_else(|| anyhow::anyhow!("Нет парсера для расширения: {}", ext))?;

                let parse_result = parser.parse(content, rel_path)?;

                // Сохраняем запись о файле
                let file_record = FileRecord {
                    id: None,
                    path: rel_path.to_string(),
                    content_hash: content_hash.to_string(),
                    ast_hash: Some(parse_result.ast_hash.clone()),
                    language: language.clone(),
                    lines_total: parse_result.lines_total,
                    indexed_at: chrono::Utc::now()
                        .format("%Y-%m-%d %H:%M:%S")
                        .to_string(),
                };
                let file_id = self.storage.upsert_file(&file_record)?;

                // Удаляем старые данные перед вставкой новых
                self.storage.delete_functions_by_file(file_id)?;
                self.storage.delete_classes_by_file(file_id)?;
                self.storage.delete_imports_by_file(file_id)?;
                self.storage.delete_calls_by_file(file_id)?;
                self.storage.delete_variables_by_file(file_id)?;

                // Конвертируем и сохраняем функции
                let functions: Vec<FunctionRecord> = parse_result
                    .functions
                    .iter()
                    .map(|f| FunctionRecord {
                        id: None,
                        file_id,
                        name: f.name.clone(),
                        qualified_name: f.qualified_name.clone(),
                        line_start: f.line_start,
                        line_end: f.line_end,
                        args: f.args.clone(),
                        return_type: f.return_type.clone(),
                        docstring: f.docstring.clone(),
                        body: f.body.clone(),
                        is_async: f.is_async,
                        node_hash: f.node_hash.clone(),
                    })
                    .collect();
                self.storage.insert_functions(&functions)?;

                // Конвертируем и сохраняем классы
                let classes: Vec<ClassRecord> = parse_result
                    .classes
                    .iter()
                    .map(|c| ClassRecord {
                        id: None,
                        file_id,
                        name: c.name.clone(),
                        line_start: c.line_start,
                        line_end: c.line_end,
                        bases: c.bases.clone(),
                        docstring: c.docstring.clone(),
                        body: c.body.clone(),
                        node_hash: c.node_hash.clone(),
                    })
                    .collect();
                self.storage.insert_classes(&classes)?;

                // Конвертируем и сохраняем импорты
                let imports: Vec<ImportRecord> = parse_result
                    .imports
                    .iter()
                    .map(|i| ImportRecord {
                        id: None,
                        file_id,
                        module: i.module.clone(),
                        name: i.name.clone(),
                        alias: i.alias.clone(),
                        line: i.line,
                        kind: i.kind.clone(),
                    })
                    .collect();
                self.storage.insert_imports(&imports)?;

                // Конвертируем и сохраняем вызовы функций
                let calls: Vec<CallRecord> = parse_result
                    .calls
                    .iter()
                    .map(|c| CallRecord {
                        id: None,
                        file_id,
                        caller: c.caller.clone(),
                        callee: c.callee.clone(),
                        line: c.line,
                    })
                    .collect();
                self.storage.insert_calls(&calls)?;

                // Конвертируем и сохраняем переменные
                let variables: Vec<VariableRecord> = parse_result
                    .variables
                    .iter()
                    .map(|v| VariableRecord {
                        id: None,
                        file_id,
                        name: v.name.clone(),
                        value: v.value.clone(),
                        line: v.line,
                    })
                    .collect();
                self.storage.insert_variables(&variables)?;
            }

            FileCategory::Text => {
                // Текстовый файл — только полнотекстовый поиск, без AST
                let text_result = TextParser::parse(content);

                let file_record = FileRecord {
                    id: None,
                    path: rel_path.to_string(),
                    content_hash: content_hash.to_string(),
                    ast_hash: None,
                    language: "text".to_string(),
                    lines_total: text_result.lines_total,
                    indexed_at: chrono::Utc::now()
                        .format("%Y-%m-%d %H:%M:%S")
                        .to_string(),
                };
                let file_id = self.storage.upsert_file(&file_record)?;

                // Удаляем старую запись текстового файла и вставляем новую
                self.storage.delete_text_file_by_file(file_id)?;

                let text_record = TextFileRecord {
                    id: None,
                    file_id,
                    content: text_result.content,
                };
                self.storage.insert_text_file(&text_record)?;
            }

            FileCategory::Binary => unreachable!("бинарные файлы не должны попасть сюда"),
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_categorize_file() {
        assert_eq!(
            file_types::categorize_file(Path::new("test.py")),
            FileCategory::Code("python".to_string())
        );
        assert_eq!(
            file_types::categorize_file(Path::new("readme.md")),
            FileCategory::Text
        );
        assert_eq!(
            file_types::categorize_file(Path::new("image.png")),
            FileCategory::Binary
        );
    }

    #[test]
    fn test_full_reindex() {
        let tmp = TempDir::new().unwrap();

        // Создаём Python-файл с функцией и классом
        fs::write(
            tmp.path().join("main.py"),
            r#"
def hello():
    """Приветствие."""
    print("Hello!")

class App:
    def run(self):
        pass
"#,
        )
        .unwrap();

        // Создаём текстовый файл
        fs::write(tmp.path().join("readme.md"), "# Project\nDescription").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let mut indexer = Indexer::new(&mut storage);
        let result = indexer.full_reindex(tmp.path(), false).unwrap();

        assert_eq!(result.files_indexed, 2, "оба файла должны быть проиндексированы");
        assert_eq!(result.files_skipped, 0, "пропущенных файлов быть не должно");
        assert_eq!(result.errors.len(), 0, "ошибок быть не должно");

        // Проверяем, что данные сохранились в БД
        let stats = storage.get_stats().unwrap();
        assert!(stats.total_functions >= 2, "минимум 2 функции: hello + run");
        assert!(stats.total_classes >= 1, "минимум 1 класс: App");
        assert!(stats.total_text_files >= 1, "минимум 1 текстовый файл: readme.md");
    }

    #[test]
    fn test_reindex_skips_unchanged() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.py"), "def foo():\n    pass\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();

        // Первая индексация
        {
            let mut indexer = Indexer::new(&mut storage);
            let r1 = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(r1.files_indexed, 1, "первый проход должен проиндексировать файл");
        }

        // Второй проход без изменений — файл должен быть пропущен
        {
            let mut indexer = Indexer::new(&mut storage);
            let r2 = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(r2.files_indexed, 0, "повторная индексация не должна записывать файл");
            assert_eq!(r2.files_skipped, 1, "файл должен быть пропущен как неизменённый");
        }
    }

    #[test]
    fn test_reindex_force_reindexes() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.py"), "def foo():\n    pass\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();

        {
            let mut indexer = Indexer::new(&mut storage);
            indexer.full_reindex(tmp.path(), false).unwrap();
        }

        // Force-режим — файл должен быть переиндексирован, даже если не изменился
        {
            let mut indexer = Indexer::new(&mut storage);
            let r = indexer.full_reindex(tmp.path(), true).unwrap();
            assert_eq!(r.files_indexed, 1, "force=true должен переиндексировать файл");
            assert_eq!(r.files_skipped, 0, "при force=true пропущенных быть не должно");
        }
    }

    #[test]
    fn test_deleted_files_removed_from_db() {
        let tmp = TempDir::new().unwrap();
        let py_path = tmp.path().join("temp.py");
        fs::write(&py_path, "def bar():\n    pass\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();

        // Индексируем файл
        {
            let mut indexer = Indexer::new(&mut storage);
            let r = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(r.files_indexed, 1);
        }

        // Удаляем файл с диска
        fs::remove_file(&py_path).unwrap();

        // Повторная индексация — запись должна исчезнуть из БД
        {
            let mut indexer = Indexer::new(&mut storage);
            let r = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(r.files_deleted, 1, "удалённый файл должен быть убран из БД");
        }

        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_files, 0, "БД должна быть пуста после удаления файла");
    }

    #[test]
    fn test_excludes_binary_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.py"), "x = 1\n").unwrap();
        // Бинарный файл — не должен попасть в индекс
        fs::write(tmp.path().join("image.png"), b"\x89PNG\r\n\x1a\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let mut indexer = Indexer::new(&mut storage);
        let r = indexer.full_reindex(tmp.path(), false).unwrap();

        // Только Python-файл проиндексирован, PNG пропущен (бинарный)
        assert_eq!(r.files_scanned, 1, "бинарные файлы не должны попасть в files_scanned");
        assert_eq!(r.files_indexed, 1);
    }

    #[test]
    fn test_excludes_target_dir() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("target")).unwrap();
        fs::write(tmp.path().join("target").join("debug.py"), "x = 1\n").unwrap();
        fs::write(tmp.path().join("main.py"), "y = 2\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let mut indexer = Indexer::new(&mut storage);
        let r = indexer.full_reindex(tmp.path(), false).unwrap();

        // Файл в target/ должен быть исключён
        assert_eq!(r.files_indexed, 1, "только main.py должен быть проиндексирован");
    }

    #[test]
    fn test_hasher_deterministic() {
        let hash1 = hasher::content_hash(b"hello world");
        let hash2 = hasher::content_hash(b"hello world");
        assert_eq!(hash1, hash2, "хеш должен быть детерминированным");

        let hash3 = hasher::content_hash(b"different content");
        assert_ne!(hash1, hash3, "разные данные дают разные хеши");
    }
}
