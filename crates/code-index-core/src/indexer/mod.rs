/// Модуль индексатора — обход директорий, определение типов файлов, хеширование
pub mod config;
pub mod file_types;
pub mod hasher;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::parser::types::ParseResult;
use crate::parser::ParserRegistry;
use crate::parser::LanguageParser;
use crate::parser::text::TextParser;
use crate::storage::models::*;
use crate::storage::Storage;
use config::IndexConfig;

/// Во сколько раз работа в памяти обходится дороже веса исходников папки —
/// значение по умолчанию.
///
/// Замеры первичной индексации с базой в памяти: сайт из 191 тыс. мелких
/// файлов (1,4 ГБ исходников) — 2,4 ГБ памяти (×1,7); типовая торговая
/// конфигурация (3,7 ГБ, 60 тыс. файлов) — 4,4 ГБ (×1,2); типовая учётная
/// (5,3 ГБ, 92 тыс. файлов) — 8,8 ГБ (×1,6). Двойка — середина этого разброса,
/// а не запас сверху: расход выше оценки встречается не реже, чем ниже.
///
/// До порционного разбора те же папки давали ×1,7–3,2, и умолчание стояло 3:
/// содержимое всех файлов и их разбор держались в памяти целиком. Теперь они
/// живут порцией (`chunk_budget_bytes`), и в пике остаются сама база и слои
/// надстройки, которые растут к концу прохода.
///
/// Значение переопределяется настройкой `memory_estimate_factor` в файле
/// настроек папки — там же и описано, когда его стоит менять.
pub const DEFAULT_MEMORY_ESTIMATE_FACTOR: f32 = 2.0;

/// Сколько весят файлы папки, которые пойдут в работу.
///
/// Нужно, чтобы решить, потянет ли машина работу с базой в памяти: у новой
/// базы размер файла нулевой и о будущем объёме ничего не говорит.
///
/// Считается тем же обходом и с теми же исключениями каталогов, что и сама
/// индексация, — иначе оценка разойдётся с тем, что будет прочитано. Читаются
/// только сведения о файлах, содержимое не открывается: на 60 тысячах файлов
/// это пара секунд при индексации в несколько минут.
pub fn estimate_source_bytes(root: &Path, config: &IndexConfig) -> u64 {
    let filter = config.clone();
    WalkDir::new(root)
        .into_iter()
        .filter_entry(move |e| {
            if e.file_type().is_dir() {
                if let Some(name) = e.file_name().to_str() {
                    return !filter.is_excluded_dir(name);
                }
            }
            true
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}
use file_types::FileCategory;

/// Результат одного прохода индексации
#[derive(Debug)]
pub struct IndexResult {
    /// Сколько файлов просмотрено (не считая бинарных)
    pub files_scanned: usize,
    /// Сколько файлов реально записано в БД (новые или изменённые)
    pub files_indexed: usize,
    /// Сколько файлов не менялось с прошлого раза (совпали время и размер
    /// либо содержимое). Эти файлы входят в `files_scanned`.
    pub files_skipped: usize,
    /// Сколько файлов не индексируется в принципе: двоичные и текстовые
    /// крупнее допустимого. В `files_scanned` они не входят — их и не
    /// просматривали дальше, поэтому смешивать эти два счётчика нельзя:
    /// сумма выходила больше числа найденных файлов и читалась как ошибка.
    pub files_not_indexable: usize,
    /// Сколько файлов удалено из БД (больше не существуют на диске)
    pub files_deleted: usize,
    /// Список ошибок: (путь, сообщение)
    pub errors: Vec<(String, String)>,
    /// Время работы в миллисекундах
    pub elapsed_ms: u64,
    /// Относительные пути записанных файлов — сырьё для точечного пересбора
    /// надстройки на старте демона вместо полного (S-6). Копятся до
    /// [`PATHS_CAP`]; на переполнении очищаются и взводится [`Self::paths_overflow`].
    pub changed_paths: Vec<String>,
    /// Относительные пути удалённых файлов — там же.
    pub deleted_paths: Vec<String>,
    /// Списки переполнились и очищены: точечный пересбор недоступен, нужен полный.
    pub paths_overflow: bool,
}

/// Потолок на списки путей в [`IndexResult`]. Смысл потолка — не копить память
/// на полной индексации (сотни тысяч файлов): за ним точечный пересбор всё
/// равно дороже полного, поэтому списки очищаются, а вызывающий идёт полным
/// путём.
pub const PATHS_CAP: usize = 10_000;

impl IndexResult {
    /// Запомнить записанный файл. После переполнения ничего не копит.
    fn note_changed(&mut self, rel_path: &str) {
        if self.paths_overflow {
            return;
        }
        if self.changed_paths.len() + self.deleted_paths.len() >= PATHS_CAP {
            self.paths_overflow = true;
            self.changed_paths.clear();
            self.deleted_paths.clear();
            return;
        }
        self.changed_paths.push(rel_path.to_string());
    }

    /// Запомнить удалённый файл. После переполнения ничего не копит.
    fn note_deleted(&mut self, rel_path: &str) {
        if self.paths_overflow {
            return;
        }
        if self.changed_paths.len() + self.deleted_paths.len() >= PATHS_CAP {
            self.paths_overflow = true;
            self.changed_paths.clear();
            self.deleted_paths.clear();
            return;
        }
        self.deleted_paths.push(rel_path.to_string());
    }
}

/// Результат параллельного парсинга одного файла
pub enum ParsedFile {
    /// Файл с исходным кодом успешно распаршен
    Code {
        rel_path: String,
        content_hash: String,
        language: String,
        lines_total: usize,
        parse_result: ParseResult,
        mtime: i64,
        file_size: i64,
        /// Для языков с двойной индексацией (html в v0.7.1) — raw-content
        /// для дополнительной записи в text_files (FTS+regex+read_file).
        /// Для остальных языков — None.
        text_for_fts: Option<String>,
        /// Phase 2 (v0.8.0): исходный content. Нужен сборщику extras
        /// (bsl-indexer) в фазе 2b; после сжатия в фазе 2c освобождается.
        raw_content: String,
        /// Content для `file_contents`, сжатый zstd параллельно в фазе 2c
        /// (v0.47.0; раньше сжималось в потоке-писателе). `None` до фазы 2c
        /// и для файлов крупнее `max_code_file_size_bytes` (oversize-запись).
        content_blob: Option<Vec<u8>>,
    },
    /// Текстовый файл (без AST)
    Text {
        rel_path: String,
        content_hash: String,
        lines_total: usize,
        content: String,
        mtime: i64,
        file_size: i64,
    },
    /// Ошибка парсинга
    Error {
        rel_path: String,
        error: String,
    },
}

/// Что писать в `file_contents` для очередного файла.
pub enum ContentInput<'a> {
    /// Сырой текст — сжимается на месте (одиночные файлы: watcher, backfill).
    Raw(&'a str),
    /// Уже сжатый blob из фазы параллельного парсинга (массовая индексация).
    Blob(&'a [u8]),
    /// Файл крупнее лимита — oversize-запись без содержимого.
    Oversize,
    /// Содержимое не сохранять.
    None,
}

/// Индексатор файловой системы
pub struct Indexer<'a> {
    storage: &'a mut Storage,
    /// Конфигурация индексатора
    config: IndexConfig,
}

impl<'a> Indexer<'a> {
    /// Создать индексатор с уже открытым хранилищем и конфигурацией по умолчанию
    pub fn new(storage: &'a mut Storage) -> Self {
        Self {
            storage,
            config: IndexConfig::default(),
        }
    }

    /// Создать индексатор с явно переданной конфигурацией
    pub fn with_config(storage: &'a mut Storage, config: IndexConfig) -> Self {
        Self { storage, config }
    }

    /// Полная переиндексация директории `root`.
    ///
    /// Если `force = true` — перезаписать все файлы независимо от хеша.
    /// Если `force = false` — пропустить файлы с неизменённым content_hash.
    ///
    /// При количестве файлов для индексации > `config.bulk_threshold` автоматически
    /// включается bulk-load режим: индексы и FTS-триггеры удаляются перед загрузкой
    /// и пересоздаются (с rebuild FTS) после — это значительно ускоряет INSERT.
    ///
    /// Парсинг (tree-sitter, CPU-bound) выполняется параллельно через rayon.
    /// Запись в SQLite (I/O-bound) — последовательно из основного потока.
    ///
    /// По завершении удаляет из БД записи файлов, которых больше нет на диске.
    pub fn full_reindex(&mut self, root: &Path, force: bool) -> Result<IndexResult> {
        self.full_reindex_with_collector(root, force, None)
    }

    /// Полная переиндексация с опциональным сборщиком extras (см.
    /// [`crate::extension::ParseExtrasCollector`]). Публичная сборка и все
    /// старые вызовы идут через `full_reindex` → `collector = None` → поведение
    /// идентично прежнему. bsl-indexer передаёт сборщик BSL: тот вытаскивает
    /// своё сырьё в фазе параллельного парсинга, пока содержимое горячее в RAM,
    /// вместо повторного чтения диска в `index_extras`.
    pub fn full_reindex_with_collector(
        &mut self,
        root: &Path,
        force: bool,
        collector: Option<&dyn crate::extension::ParseExtrasCollector>,
    ) -> Result<IndexResult> {
        let start = std::time::Instant::now();
        let mut result = IndexResult {
            files_scanned: 0,
            files_indexed: 0,
            files_skipped: 0,
            files_not_indexable: 0,
            files_deleted: 0,
            errors: vec![],
            elapsed_ms: 0,
            changed_paths: Vec::new(),
            deleted_paths: Vec::new(),
            paths_overflow: false,
        };

        // ── Этап 0: загрузка состояния БД ─────────────────────────────────────
        // Тип: path → (id, content_hash, mtime, file_size)
        let existing_files: HashMap<String, (i64, String, Option<i64>, Option<i64>)> = self
            .storage
            .get_all_files()?
            .into_iter()
            .filter_map(|f| {
                f.id.map(|id| (f.path.clone(), (id, f.content_hash.clone(), f.mtime, f.file_size)))
            })
            .collect();

        // Определяем: это первичная индексация (пустая БД) или обновление
        let is_fresh_db = existing_files.is_empty();

        // Осталась ли отметка о незавершённой массовой загрузке. Она значит,
        // что прошлый проход убили посередине: часть файлов в базе есть, но
        // индексов и полнотекстового поиска на них нет. Такой проход дочитывает
        // остаток и обязан достроить индексы, чем бы дело ни кончилось по числу
        // файлов.
        let resume = self.storage.bulk_in_progress();
        if resume {
            tracing::warn!(
                "[{}] прошлая загрузка не была завершена: записанные файлы в базе есть, \
                 но индексы и полнотекстовый поиск на них не построены — дочитываю остаток",
                root.display()
            );
        }

        // Сборщик extras участвует ТОЛЬКО в полном парсинге (--force или свежая
        // БД): тогда парсятся все файлы и его полный DELETE+rebuild корректен.
        // При частичном mtime-fast-path (демон с изменениями) сборщик выключаем
        // — extras-слои пересобирает index_extras как раньше (с диска). Сюда же
        // попадает продолжение прерванной загрузки: разбирается только остаток
        // файлов, и слой, собранный по нему одному, был бы неполным.
        let collector = if force || is_fresh_db { collector } else { None };

        // ── Этап 1: обход дерева (без чтения содержимого) ────────────────────
        // О начале каждого тяжёлого этапа сообщаем ДО его выполнения: если
        // этап встанет, в журнале останется имя того, на чём встали, а не
        // тишина после отчёта о предыдущем этапе.
        if is_fresh_db || force {
            tracing::info!(
                "[{}] обхожу дерево каталогов: база пустая, поэтому в работу пойдёт каждый файл",
                root.display()
            );
        } else {
            tracing::info!(
                "[{}] обхожу дерево каталогов: у каждого файла сверяю время изменения и размер \
                 с тем, что записано в базе (содержимое не читается)",
                root.display()
            );
        }
        crate::logging::stage_begin("обход дерева и сверка файлов");
        let candidates_start = std::time::Instant::now();
        let (entries, seen_paths) = self.collect_entries(root, &mut result)?;
        let entries_to_read = self.filter_entries_by_mtime(&entries, force, &existing_files, &mut result);
        let candidates_dur = candidates_start.elapsed();
        tracing::info!(
            "обход дерева закончен за {} мс: просмотрено {} файлов, к чтению {}",
            candidates_dur.as_millis(),
            result.files_scanned,
            entries_to_read.len()
        );
        crate::logging::stage_detail(format!(
            "просмотрено {}, к чтению {}",
            result.files_scanned,
            entries_to_read.len()
        ));
        crate::logging::stage_done("обход дерева и сверка файлов", candidates_dur);

        // Файлы, у которых изменились только время и размер: содержимое то же,
        // переиндексировать нечего — обновляются одни сведения о файле. Копятся
        // по всем порциям, применяются одним проходом в конце.
        let mut metadata_updates: Vec<(String, i64, i64)> = Vec::new();

        // Порционный разбор нужен там, где в работу идёт всё дерево: первичная
        // индексация, принудительный полный разбор и продолжение прерванной
        // загрузки. Только в этих случаях список кандидатов известен заранее —
        // значит и решение о пакетном режиме, и удаление прежних строк можно
        // сделать до цикла, один раз, пока вторичные индексы ещё на месте.
        //
        // На обычных правках (наблюдатель, сверка при старте) файлов мало,
        // деление на порции ничего не экономит, а число кандидатов там заранее
        // неизвестно — оно выясняется только сверкой содержимого.
        let chunked = (is_fresh_db || force || resume) && self.config.chunk_budget_bytes > 0;

        // Непорционный путь читает всех кандидатов сразу: от их числа зависит и
        // пакетный режим, и пакетное удаление прежних строк.
        let preread: Option<Vec<Candidate>> = if chunked {
            None
        } else {
            Some(self.read_entries(
                &entries_to_read,
                force,
                &existing_files,
                &mut result,
                &mut metadata_updates,
            ))
        };

        // Изменений нет — дальше все этапы отработают вхолостую и напечатают
        // по строке нулей каждый. Читать в них нечего, поэтому выходим сразу:
        // вызывающий напечатает единственную осмысленную строку итога. При
        // недостроенной базе так выйти нельзя — индексы всё равно надо поднять.
        let nothing_to_do = match &preread {
            Some(candidates) => candidates.is_empty(),
            None => entries_to_read.is_empty(),
        };
        if nothing_to_do && metadata_updates.is_empty() && !resume {
            let ничего_не_исчезло = !existing_files.keys().any(|p| !seen_paths.contains(p));
            if ничего_не_исчезло {
                result.elapsed_ms = start.elapsed().as_millis() as u64;
                return Ok(result);
            }
        }

        let candidate_count = match &preread {
            Some(candidates) => candidates.len(),
            None => entries_to_read.len(),
        };

        // Включаем bulk-load если количество файлов для индексации превышает порог.
        // При продолжении прерванной загрузки — принудительно: индексов в базе
        // всё равно нет, и достроить их обязан этот проход, сколько бы файлов
        // ни осталось дочитать.
        let bulk_mode = resume || candidate_count > self.config.bulk_threshold;

        // В bulk-режиме построчный DELETE в фазе записи пропускаем:
        //   • свежая БД — удалять нечего;
        //   • обновление непустой БД — старые строки кандидатов чистим одним
        //     пакетным проходом ниже, пока индексы idx_*_file ещё живы.
        let skip_delete = bulk_mode || is_fresh_db;

        // Отметку ставим ДО снятия индексов и снимаем после того, как они
        // пересозданы: всё это время база недостроена, и обрыв посередине
        // должен быть виден следующему запуску.
        if bulk_mode {
            self.storage.set_bulk_in_progress(true)?;
        }

        if bulk_mode && (is_fresh_db || resume) {
            // Первичная индексация: таблицы уже созданы через initialize(),
            // дропаем индексы которые были созданы вместе со схемой
            tracing::info!(
                "[пакетный режим] первичная индексация {} файлов (порог {}): индексы временно снимаются",
                candidate_count,
                self.config.bulk_threshold
            );
            self.storage.prepare_bulk_load()?;
        } else if bulk_mode {
            // Обновление существующей БД. Порядок критичен для скорости:
            //   1) снять FTS-триггеры functions/classes, чтобы пакетный DELETE
            //      не дёргал их построчно (второй скрытый тормоз);
            //   2) удалить старые строки кандидатов одним пакетным проходом,
            //      ПОКА вторичные индексы idx_*_file живы (иначе построчный
            //      DELETE вырождается в полный скан таблиц на каждый файл —
            //      квадратичная деградация);
            //   3) дропнуть B-tree индексы перед массовой вставкой.
            tracing::info!(
                "[пакетный режим] обновление {} файлов (порог {}): пакетное удаление прежних строк и снятие индексов",
                candidate_count,
                self.config.bulk_threshold
            );
            self.storage.drop_fts_triggers()?;
            // Только те кандидаты, что реально есть в БД (у новых файлов старых
            // строк нет). file_id берём из уже загруженной карты existing_files.
            let victim_ids: Vec<i64> = match &preread {
                Some(candidates) => candidates
                    .iter()
                    .filter_map(|c| existing_files.get(c.0.as_str()).map(|e| e.0))
                    .collect(),
                // Порционный путь доходит сюда только при принудительном полном
                // разборе: сверка содержимого тогда не делается, поэтому
                // кандидаты — ровно все найденные обходом файлы, и прежние
                // строки каждого из них надо убрать заранее.
                None => entries_to_read
                    .iter()
                    .filter_map(|e| existing_files.get(e.rel_path.as_str()).map(|v| v.0))
                    .collect(),
            };
            tracing::info!(
                "[пакетный режим] пакетное удаление прежних строк: {} из {} кандидатов уже были в базе",
                victim_ids.len(),
                candidate_count
            );
            self.storage.delete_file_data_bulk(&victim_ids)?;
            self.storage.prepare_bulk_load()?;
        }

        // Создаём реестр парсеров из конфигурации — один раз для всего прохода.
        // ParserRegistry содержит HashMap<String, Arc<dyn LanguageParser>>.
        // LanguageParser: Send + Sync, Arc: Send + Sync, HashMap: Send+Sync →
        // ParserRegistry: Send + Sync, что требуется для par_iter.
        let registry = ParserRegistry::from_languages(&self.config.languages);

        // Сборщик extras очищает свои слои один раз, до первой порции: сброс
        // теперь идёт после каждой порции, и очистка внутри него стирала бы
        // собранное предыдущими.
        if let Some(collector) = collector {
            collector.begin(&mut *self.storage)?;
        }

        // ── Этапы 2–3: чтение, разбор, сжатие и запись ───────────────────────
        // Порция за порцией: память, занятая содержимым и разбором, живёт
        // только внутри одного захода. Именно это, а не место базы, держит
        // потолок расхода — в памяти база или на диске, содержимое всё равно
        // читается и разбирается целиком.
        match preread {
            Some(candidates) => {
                self.process_chunk(
                    candidates,
                    &registry,
                    collector,
                    ChunkPolicy { skip_delete, existing_files: None, label: None },
                    &mut result,
                )?;
            }
            None => {
                let chunks = chunk_by_budget(&entries_to_read, self.config.chunk_budget_bytes);
                let total = chunks.len();
                if total > 1 {
                    tracing::info!(
                        "файлы пойдут порциями: {} порций примерно по {} — память освобождается после каждой",
                        total,
                        crate::logging::human_bytes(self.config.chunk_budget_bytes as u64)
                    );
                }
                for (i, chunk) in chunks.into_iter().enumerate() {
                    let candidates = self.read_entries(
                        chunk,
                        force,
                        &existing_files,
                        &mut result,
                        &mut metadata_updates,
                    );
                    self.process_chunk(
                        candidates,
                        &registry,
                        collector,
                        ChunkPolicy {
                            skip_delete,
                            // При продолжении прерванной загрузки пакетного
                            // удаления не было, а часть файлов в базе уже есть:
                            // такие чистят свои прежние строки сами.
                            existing_files: if resume { Some(&existing_files) } else { None },
                            label: if total > 1 { Some((i + 1, total)) } else { None },
                        },
                        &mut result,
                    )?;
                }
            }
        }

        // Сброс остатка сырья сборщика extras и отметка «слои построены» —
        // серийно, после последней порции.
        if let Some(collector) = collector {
            collector.write(&mut *self.storage)?;
        }

        // Обновляем mtime/file_size для файлов с неизменённым содержимым.
        if !metadata_updates.is_empty() {
            self.storage.begin_batch()?;
            // Провал здесь не портит данные, но означает, что на следующем
            // старте файл не пройдёт быстрый путь по времени изменения и будет
            // разобран целиком. Молчать об этом нельзя: замедление выглядит
            // беспричинным (G-5).
            let mut meta_errors = 0usize;
            for (path, mtime, file_size) in &metadata_updates {
                if let Err(e) = self.storage.update_file_metadata(path, *mtime, *file_size) {
                    meta_errors += 1;
                    if meta_errors <= 5 {
                        tracing::debug!("не обновлены сведения о файле {}: {}", path, e);
                    }
                }
            }
            self.storage.commit_batch()?;
            if meta_errors > 0 {
                tracing::warn!(
                    "сведения о файлах не обновлены у {} из {} — на следующем старте \
                     они пойдут полным разбором вместо быстрого пути",
                    meta_errors,
                    metadata_updates.len()
                );
            }
        }

        // ── Этап 4: индексы + FTS rebuild ────────────────────────────────────
        // Завершаем bulk-load: пересоздаём индексы, триггеры, rebuild FTS.
        // Делается ОДИН раз на весь проход, а не на каждую порцию: пересоздание
        // индексов и перестройка полнотекстового поиска идут по всей базе, и на
        // каждой порции это выродилось бы в квадратичную работу.
        if bulk_mode {
            let idx_start = std::time::Instant::now();
            tracing::info!("создаю индексы и перестраиваю полнотекстовый поиск");
            self.storage.finish_bulk_load()?;
            let idx_dur = idx_start.elapsed();
            let idx_ms = idx_dur.as_millis();
            tracing::info!("индексы и полнотекстовый поиск готовы за {} мс", idx_ms);
            crate::logging::stage_done("индексы и полнотекстовый поиск", idx_dur);
            // База снова согласована — отметку о незавершённой загрузке снимаем.
            self.storage.set_bulk_in_progress(false)?;
        }

        // ── Этап 5: удаление устаревших записей ──────────────────────────────
        // seen_paths уже собран в Этапе 1 — повторный обход дерева не нужен
        let cleanup_start = std::time::Instant::now();

        // Удаляем из БД файлы, которых больше нет на диске — в одной транзакции
        self.storage.begin_batch()?;
        for (path, (id, _, _, _)) in &existing_files {
            if !seen_paths.contains(path) {
                self.storage.delete_file(*id)?;
                result.files_deleted += 1;
                result.note_deleted(path);
            }
        }
        self.storage.commit_batch()?;
        let cleanup_dur = cleanup_start.elapsed();
        let cleanup_ms = cleanup_dur.as_millis();
        if result.files_deleted > 0 {
            tracing::info!("[этап 5] удаление исчезнувших файлов: {} мс ({} файлов)", cleanup_ms, result.files_deleted);
            crate::logging::stage_detail(format!(
                "{} удалено",
                crate::logging::plural(result.files_deleted as u64, "файл", "файла", "файлов")
            ));
            crate::logging::stage_done("удаление исчезнувших", cleanup_dur);
        }
        // ── Этап 6: Phase 2 backfill для file_contents ──────────────────────
        // Отдельная фаза от write-step — она работает только для файлов,
        // у которых hash изменился (write_code_to_db уже вызвал upsert_file_content).
        // Здесь же добиваем все остальные code-файлы (mtime+hash тот же, в files
        // запись есть, но file_contents для них пуст). Это типичная ситуация
        // первого запуска v0.8.0 на БД от v0.7.x: файлы стабильны, никто не
        // зашёл в write_code_to_db, и backfill делает однократный обход.
        //
        // Промежуточные commit'ы каждые batch_size строк — иначе на 90K-репо
        // WAL раздуется до многих ГБ.
        let backfill_candidates = self.storage.list_code_files_without_content()?;
        if !backfill_candidates.is_empty() {
            let backfill_start = std::time::Instant::now();
            let mut backfilled = 0usize;
            let mut backfill_errors = 0usize;
            let mut in_batch = 0usize;
            let backfill_batch_size = self.config.batch_size.max(500);
            self.storage.begin_batch()?;
            for (file_id, path) in &backfill_candidates {
                let abs = root.join(path);
                match std::fs::read_to_string(&abs) {
                    Ok(content) => {
                        match self.storage.upsert_file_content(
                            *file_id,
                            &content,
                            self.config.max_code_file_size_bytes,
                        ) {
                            Ok(_) => backfilled += 1,
                            Err(e) => {
                                tracing::debug!("[этап 6] не записано содержимое {}: {}", path, e);
                                backfill_errors += 1;
                            }
                        }
                    }
                    Err(_) => {
                        // Файл нечитаемый — пропускаем тихо.
                        backfill_errors += 1;
                    }
                }
                in_batch += 1;
                if in_batch >= backfill_batch_size {
                    self.storage.commit_batch()?;
                    self.storage.begin_batch()?;
                    in_batch = 0;
                }
            }
            self.storage.commit_batch()?;
            let backfill_dur = backfill_start.elapsed();
            let backfill_ms = backfill_dur.as_millis();
            crate::logging::stage_detail(format!(
                "{} наполнено",
                crate::logging::plural(backfilled as u64, "файл", "файла", "файлов")
            ));
            crate::logging::stage_done("дозаполнение содержимого", backfill_dur);
            tracing::info!(
                "[этап 6] дозаполнение содержимого файлов: {} мс ({} наполнено из {} кандидатов, {} ошибок)",
                backfill_ms,
                backfilled,
                backfill_candidates.len(),
                backfill_errors
            );
        }

        result.elapsed_ms = start.elapsed().as_millis() as u64;
        tracing::info!("работа с базой заняла {} мс", result.elapsed_ms);
        Ok(result)
    }


    /// Разобрать, сжать и записать одну порцию файлов.
    ///
    /// Всё, что порция занимает в памяти — содержимое, разбор, сжатые копии —
    /// живёт внутри вызова и освобождается на выходе. Поэтому расход зависит от
    /// бюджета порции, а не от того, сколько весит папка целиком.
    fn process_chunk(
        &mut self,
        candidates: Vec<Candidate>,
        registry: &ParserRegistry,
        collector: Option<&dyn crate::extension::ParseExtrasCollector>,
        policy: ChunkPolicy<'_>,
        result: &mut IndexResult,
    ) -> Result<()> {
        if candidates.is_empty() {
            return Ok(());
        }
        // Пометка порции в журнале: без неё на большом дереве строки этапов
        // повторяются десятками и непонятно, к какой порции какая относится.
        let tag = match policy.label {
            Some((n, total)) => format!("[порция {} из {}] ", n, total),
            None => String::new(),
        };

        // ── Этап 2: параллельный парсинг (CPU-bound) ─────────────────────────
        // tree-sitter парсинг выполняется в нескольких потоках через rayon.
        // Чтение файлов уже выполнено в read_entries — здесь только AST.
        tracing::info!("{}разбираю {} изменившихся файлов в несколько потоков", tag, candidates.len());
        let parse_start = std::time::Instant::now();
        // Лимит для `file_contents` — копия в замыкание: сжатие идёт здесь же,
        // в rayon-потоках, а не в единственном потоке-писателе (v0.47.0).
        let max_code_size = self.config.max_code_file_size_bytes;
        // Содержимое ПЕРЕДАЁТСЯ в разбор, а не копируется: раньше здесь стоял
        // clone, и весь прочитанный текст жил в памяти дважды.
        let mut parse_results: Vec<ParsedFile> = candidates
            .into_par_iter()
            .map(|(rel_path, content, hash, category, mtime, file_size)| {
                match category {
                    FileCategory::Code(language) => {
                        // Определяем парсер по расширению файла
                        let ext = Path::new(rel_path.as_str())
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("")
                            .to_lowercase();

                        match registry.get_parser(&ext) {
                            Some(parser) => {
                                match parser.parse_guarded(&content, &rel_path) {
                                    Ok(pr) => ParsedFile::Code {
                                        content_hash: hash,
                                        lines_total: pr.lines_total,
                                        parse_result: pr,
                                        mtime,
                                        file_size,
                                        text_for_fts: if file_types::is_dual_indexed_language(&language) {
                                            Some(content.clone())
                                        } else {
                                            None
                                        },
                                        language,
                                        rel_path,
                                        raw_content: content,
                                        content_blob: None,
                                    },
                                    Err(e) => ParsedFile::Error {
                                        rel_path,
                                        error: e.to_string(),
                                    },
                                }
                            }
                            None => ParsedFile::Error {
                                rel_path,
                                error: format!("Нет парсера для расширения: {}", ext),
                            },
                        }
                    }
                    FileCategory::Text => {
                        // Проверяем: это XML-файл выгрузки 1С?
                        let is_xml = Path::new(rel_path.as_str())
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|e| e == "xml")
                            .unwrap_or(false);
                        if is_xml {
                            let xml_parser = crate::parser::xml_1c::Xml1CParser;
                            if let Ok(pr) = xml_parser.parse(&content, &rel_path) {
                                if !pr.functions.is_empty()
                                    || !pr.classes.is_empty()
                                    || !pr.variables.is_empty()
                                {
                                    return ParsedFile::Code {
                                        rel_path,
                                        content_hash: hash,
                                        language: "xml_1c".to_string(),
                                        lines_total: pr.lines_total,
                                        parse_result: pr,
                                        mtime,
                                        file_size,
                                        text_for_fts: None,
                                        raw_content: content,
                                        content_blob: None,
                                    };
                                }
                            }
                        }
                        // Fallback: текстовая индексация
                        let text_result = TextParser::parse(&content);
                        ParsedFile::Text {
                            rel_path,
                            content_hash: hash,
                            lines_total: text_result.lines_total,
                            content: text_result.content,
                            mtime,
                            file_size,
                        }
                    }
                    FileCategory::Binary => unreachable!("бинарные файлы не должны попасть сюда"),
                }
            })
            .collect();
        let parse_dur = parse_start.elapsed();
        tracing::info!("{}разбор закончен за {} мс ({} файлов)", tag, parse_dur.as_millis(), parse_results.len());
        crate::logging::stage_detail(format!(
            "{} разобрано",
            crate::logging::plural(parse_results.len() as u64, "файл", "файла", "файлов")
        ));
        crate::logging::stage_done("разбор файлов", parse_dur);

        // ── Этап 2b: сбор extras-сырья (bsl-indexer) ─────────────────────────
        // Пока parse_results ещё горячие в RAM — параллельно отдаём каждый
        // файл сборщику расширения (обращения к объектам, комментарии, XML).
        // Диск не перечитывается. Для универсальной сборки collector = None →
        // проход пропускается, накладных расходов ноль.
        if let Some(collector) = collector {
            use crate::extension::ParsedFileCtx;
            parse_results.par_iter().for_each(|pf| match pf {
                ParsedFile::Code { rel_path, language, parse_result, raw_content, .. } => {
                    collector.on_parsed(ParsedFileCtx {
                        rel_path,
                        language,
                        content: raw_content,
                        parse_result: Some(parse_result),
                    });
                }
                ParsedFile::Text { rel_path, content, .. } => {
                    collector.on_parsed(ParsedFileCtx {
                        rel_path,
                        language: "text",
                        content,
                        parse_result: None,
                    });
                }
                ParsedFile::Error { .. } => {}
            });
        }

        // ── Этап 2c: параллельное сжатие content (v0.47.0) ───────────────────
        // Раньше zstd вызывался в фазе записи, то есть в единственном потоке-
        // писателе: на боевом PHP-сайте (151 тыс. файлов) это 37 с из 65 с
        // записи. Здесь то же сжатие раскладывается на все ядра rayon, а
        // исходная строка сразу освобождается — пик RAM не растёт.
        // Сборщику extras (фаза 2b) сырой content уже отдан.
        let compress_start = std::time::Instant::now();
        // Компрессор создаётся ОДИН раз на поток (`for_each_init`), а не на файл:
        // при 151 тыс. мелких файлов инициализация zstd-контекста на каждый вызов
        // стоила дороже самого сжатия. `clear()` без `shrink_to_fit()` — намеренно:
        // возврат буфера аллокатору на каждой итерации сериализует потоки.
        parse_results.par_iter_mut().for_each_init(
            || zstd::bulk::Compressor::new(Storage::FILE_CONTENTS_ZSTD_LEVEL).ok(),
            |compressor, pf| {
                if let ParsedFile::Code { raw_content, content_blob, .. } = pf {
                    *content_blob = if raw_content.len() > max_code_size {
                        None
                    } else {
                        match compressor {
                            Some(c) => c.compress(raw_content.as_bytes()).ok(),
                            None => Storage::compress_content(raw_content).ok(),
                        }
                    };
                    raw_content.clear();
                    raw_content.shrink_to_fit();
                }
            },
        );
        let compress_dur = compress_start.elapsed();
        tracing::info!("{}содержимое файлов сжато за {} мс", tag, compress_dur.as_millis());
        crate::logging::stage_done("сжатие содержимого", compress_dur);

        // ── Этап 3: последовательная запись в SQLite ──────────────────────────
        // SQLite не поддерживает параллельную запись — пишем из основного потока.
        tracing::info!("{}записываю разобранное в базу", tag);
        let write_start = std::time::Instant::now();
        let batch_size = self.config.batch_size;
        let mut batch_count = 0usize;
        let indexed_before = result.files_indexed;

        // Открываем первую транзакцию перед началом цикла
        self.storage.begin_batch()?;

        // Прогресс — по времени, а не по размеру транзакции: шаг в файлах на
        // лёгких файлах сыплет строками, на тяжёлых молчит минутами, а на
        // репозитории меньше batch_size файлов не печатает ничего.
        let mut progress = crate::logging::Heartbeat::every_secs(5);
        for parsed in &parse_results {
            let total_processed = result.files_indexed + result.errors.len();
            if total_processed > 0 && progress.due() {
                tracing::info!(
                    "{}записано в базу {} из {} изменившихся файлов",
                    tag,
                    result.files_indexed - indexed_before,
                    parse_results.len()
                );
            }

            match parsed {
                ParsedFile::Code {
                    rel_path,
                    content_hash,
                    language,
                    lines_total,
                    parse_result,
                    mtime,
                    file_size,
                    text_for_fts,
                    content_blob,
                    raw_content: _,
                } => {
                    match self.write_code_to_db(
                        rel_path,
                        content_hash,
                        language,
                        *lines_total,
                        parse_result,
                        policy.skip_delete_for(rel_path),
                        Some(*mtime),
                        Some(*file_size),
                        text_for_fts.as_deref(),
                        match content_blob {
                            Some(b) => ContentInput::Blob(b),
                            None => ContentInput::Oversize,
                        },
                    ) {
                        Ok(_) => {
                            result.files_indexed += 1;
                            result.note_changed(rel_path);
                            batch_count += 1;
                        }
                        Err(e) => {
                            result.errors.push((rel_path.clone(), e.to_string()));
                        }
                    }
                }
                ParsedFile::Text {
                    rel_path,
                    content_hash,
                    lines_total,
                    content,
                    mtime,
                    file_size,
                } => {
                    match self.write_text_to_db(rel_path, content_hash, *lines_total, content, policy.skip_delete_for(rel_path), Some(*mtime), Some(*file_size)) {
                        Ok(_) => {
                            result.files_indexed += 1;
                            result.note_changed(rel_path);
                            batch_count += 1;
                        }
                        Err(e) => {
                            result.errors.push((rel_path.clone(), e.to_string()));
                        }
                    }
                }
                ParsedFile::Error { rel_path, error } => {
                    result.errors.push((rel_path.clone(), error.clone()));
                }
            }

            // Коммитим накопленный батч и открываем новую транзакцию
            if batch_count >= batch_size {
                self.storage.commit_batch()?;
                self.storage.begin_batch()?;
                batch_count = 0;
            }
        }

        // Коммитим оставшиеся записи последнего неполного батча
        self.storage.commit_batch()?;
        let write_dur = write_start.elapsed();
        tracing::info!(
            "{}запись в базу закончена за {} мс ({} файлов)",
            tag,
            write_dur.as_millis(),
            result.files_indexed - indexed_before
        );
        crate::logging::stage_detail(format!(
            "{} записано, {} без изменений",
            result.files_indexed, result.files_skipped
        ));
        crate::logging::stage_done("запись в базу", write_dur);

        // Разобранное больше не нужно: освобождаем до возврата, чтобы следующая
        // порция начинала с чистой памяти.
        drop(parse_results);

        // Сырьё сборщика extras сбрасывается сразу за порцией — иначе оно
        // копилось бы по всему дереву и деление на порции не удержало бы расход.
        if let Some(collector) = collector {
            collector.flush(&mut *self.storage)?;
        }

        Ok(())
    }
    /// Записать код-файл в БД: метаданные + символы (функции, классы, импорты и т.д.)
    /// skip_delete: при первичной индексации пропускать DELETE (БД пуста, удалять нечего)
    /// content: Phase 2 (v0.8.0). `Raw` — сжать на месте (одиночные файлы),
    /// `Blob` — content уже сжат в фазе параллельного парсинга (v0.47.0),
    /// `Oversize` — файл крупнее `config.max_code_file_size_bytes`,
    /// `None` — не сохранять содержимое (тесты и места, где content недоступен).
    pub fn write_code_to_db(
        &self,
        rel_path: &str,
        content_hash: &str,
        language: &str,
        lines_total: usize,
        parse_result: &ParseResult,
        skip_delete: bool,
        mtime: Option<i64>,
        file_size: Option<i64>,
        // Для языков с двойной индексацией (html в v0.7.1) — raw-content,
        // который дополнительно записывается в text_files. Для остальных — None.
        text_for_fts: Option<&str>,
        // Phase 2: content для записи в `file_contents` (см. `ContentInput`).
        content: ContentInput<'_>,
    ) -> Result<()> {
        // Сохраняем запись о файле
        let file_record = FileRecord {
            id: None,
            path: rel_path.to_string(),
            content_hash: content_hash.to_string(),
            language: language.to_string(),
            lines_total,
            indexed_at: chrono::Utc::now()
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            mtime,
            file_size,
        };
        let file_id = self.storage.upsert_file(&file_record)?;

        // Удаляем старые данные перед вставкой новых
        // При первичной индексации (skip_delete) — пропускаем, БД пуста
        if !skip_delete {
            self.storage.delete_functions_by_file(file_id)?;
            self.storage.delete_classes_by_file(file_id)?;
            self.storage.delete_imports_by_file(file_id)?;
            self.storage.delete_calls_by_file(file_id)?;
            self.storage.delete_variables_by_file(file_id)?;
            // Для языков с двойной индексацией убираем старую запись text_files,
            // чтобы не дублировать при upsert.
            if text_for_fts.is_some() {
                self.storage.delete_text_file_by_file(file_id)?;
            }
        }

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
                // Поля переопределения BSL-расширения (для других языков = None)
                override_type: f.override_type.clone(),
                override_target: f.override_target.clone(),
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

        // Двойная индексация: для html (и других языков из is_dual_indexed_language)
        // дополнительно сохраняем сырой контент в text_files, чтобы продолжали
        // работать search_text/grep_text/read_file как раньше.
        // Для dual-indexed языков content хранится дважды — в `file_contents` и
        // в `text_contents`. Сжимаем его ОДИН раз: готовый blob кладём в обе
        // таблицы (v0.47.0; до этого один и тот же текст жался дважды).
        if let Some(fts_text) = text_for_fts {
            match content {
                ContentInput::Blob(b) => self.storage.insert_text_file_blob(file_id, b, fts_text)?,
                _ => self.storage.insert_text_file(&crate::storage::models::TextFileRecord {
                    id: None,
                    file_id,
                    content: fts_text.to_string(),
                })?,
            }
        }

        // Phase 2: сохраняем content в `file_contents` (zstd).
        // Файлы крупнее `max_code_file_size_bytes` получают oversize-запись (без blob).
        match content {
            ContentInput::Raw(raw) => self.storage.upsert_file_content(
                file_id,
                raw,
                self.config.max_code_file_size_bytes,
            )?,
            ContentInput::Blob(b) => self.storage.upsert_file_content_blob(file_id, Some(b))?,
            ContentInput::Oversize => self.storage.upsert_file_content_blob(file_id, None)?,
            ContentInput::None => {}
        }

        Ok(())
    }

    /// Записать текстовый файл в БД: метаданные + полное содержимое для FTS
    pub fn write_text_to_db(
        &self,
        rel_path: &str,
        content_hash: &str,
        lines_total: usize,
        content: &str,
        skip_delete: bool,
        mtime: Option<i64>,
        file_size: Option<i64>,
    ) -> Result<()> {
        let file_record = FileRecord {
            id: None,
            path: rel_path.to_string(),
            content_hash: content_hash.to_string(),
            language: "text".to_string(),
            lines_total,
            indexed_at: chrono::Utc::now()
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            mtime,
            file_size,
        };
        let file_id = self.storage.upsert_file(&file_record)?;

        // Удаляем старую запись текстового файла и вставляем новую
        if !skip_delete {
            self.storage.delete_text_file_by_file(file_id)?;
        }

        let text_record = TextFileRecord {
            id: None,
            file_id,
            content: content.to_string(),
        };
        self.storage.insert_text_file(&text_record)?;

        Ok(())
    }

    /// Первый проход: обойти директорию, собрать список файлов для индексации.
    ///
    /// Трёхфазный сбор:
    /// 1a. WalkDir — быстрый обход, собрать пути + metadata (mtime/size) без чтения содержимого
    /// 1b. mtime/size pre-filter — пропустить файлы, где mtime+size совпадают с БД
    /// 1c. rayon par_iter — параллельное чтение + SHA-256 хеш только изменённых файлов
    /// 1d. hash comparison — пропустить файлы с неизменённым хешем, собрать metadata_updates
    ///
    /// Возвращает (candidates, seen_paths, metadata_updates).
    /// seen_paths используется для очистки удалённых файлов без повторного обхода дерева.
    /// metadata_updates содержит файлы, у которых хеш не изменился, но mtime/size обновились.
    /// Первый проход: обойти директорию и собрать список файлов — пути,
    /// категорию, время изменения и размер. Содержимое НЕ читается.
    ///
    /// Возвращает (entries, seen_paths). seen_paths используется для очистки
    /// удалённых файлов без повторного обхода дерева.
    fn collect_entries(
        &self,
        root: &Path,
        result: &mut IndexResult,
    ) -> Result<(Vec<FileEntry>, HashSet<String>)> {
        let config_for_filter = self.config.clone();
        let file_matcher = self.config.build_file_exclude_matcher();

        let walker = WalkDir::new(root).into_iter().filter_entry(move |e| {
            if e.file_type().is_dir() {
                if let Some(name) = e.file_name().to_str() {
                    return !config_for_filter.is_excluded_dir(name);
                }
            }
            true
        });

        let mut entries: Vec<FileEntry> = Vec::new();
        let mut seen_paths: HashSet<String> = HashSet::new();

        for entry in walker.filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }

            // Проверяем лимит количества файлов (0 = без лимита)
            if self.config.max_files > 0 && result.files_scanned >= self.config.max_files {
                break;
            }

            let path = entry.path();

            // Проверяем exclude_file_patterns по имени файла
            if let Some(fname) = path.file_name().and_then(|f| f.to_str()) {
                if file_matcher.is_match(fname) {
                    continue;
                }
            }

            let category =
                file_types::categorize_file_in_repo(
                    path,
                    self.config.repo_language.as_deref(),
                    &self.config.extra_text_extensions,
                );

            if matches!(category, FileCategory::Binary) {
                continue;
            }

            // Получаем метаданные для всех файлов
            let meta = entry.metadata().ok();

            // Лимит размера — только для текстовых файлов, код индексируем всегда.
            // Исключение — файлы выгрузки 1С, по которым реально ищут: оглавление
            // конфигурации, права ролей, структура формы. В крупных конфигурациях
            // они перерастают мегабайт (в типовой торговой Configuration.xml — 1,2 МБ,
            // Rights.xml до 5 МБ) и молча выпадали из индекса целиком. Макеты печатных
            // форм (Template.xml, до 78 МБ) и служебная опись выгрузки под исключение НЕ
            // подпадают: искать по ним нечего, а места занимают больше всего.
            if !matches!(category, FileCategory::Code(_))
                && !file_types::is_size_exempt(path)
            {
                if let Some(ref m) = meta {
                    if m.len() as usize > self.config.max_file_size {
                        result.files_not_indexable += 1;
                        continue;
                    }
                }
            }

            // mtime и file_size для быстрой проверки изменений
            let mtime = meta.as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let file_size_val = meta.as_ref().map(|m| m.len() as i64).unwrap_or(0);

            let rel_path = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");

            result.files_scanned += 1;
            seen_paths.insert(rel_path.clone());
            entries.push(FileEntry {
                abs_path: path.to_path_buf(),
                rel_path,
                category,
                mtime,
                file_size: file_size_val,
            });
        }

        Ok((entries, seen_paths))
    }

    /// Быстрая фильтрация по времени изменения и размеру — без чтения файлов.
    /// Совпали оба с записанными в базе — файл считается неизменившимся.
    fn filter_entries_by_mtime<'e>(
        &self,
        entries: &'e [FileEntry],
        force: bool,
        existing_files: &HashMap<String, (i64, String, Option<i64>, Option<i64>)>,
        result: &mut IndexResult,
    ) -> Vec<&'e FileEntry> {
        if force {
            return entries.iter().collect();
        }
        let mut to_read = Vec::new();
        let mut skipped = 0usize;
        for entry in entries {
            match existing_files.get(&entry.rel_path) {
                Some((_, _, Some(stored_mtime), Some(stored_size)))
                    if *stored_mtime == entry.mtime && *stored_size == entry.file_size =>
                {
                    skipped += 1;
                }
                _ => to_read.push(entry),
            }
        }
        result.files_skipped += skipped;
        to_read
    }

    /// Прочитать и захешировать порцию файлов (параллельно, rayon), отсеяв те,
    /// у которых содержимое не изменилось.
    ///
    /// Возвращает кандидатов на индексацию; файлы с прежним содержимым уходят в
    /// `metadata_updates` — им нужно обновить только время и размер.
    fn read_entries(
        &self,
        entries: &[&FileEntry],
        force: bool,
        existing_files: &HashMap<String, (i64, String, Option<i64>, Option<i64>)>,
        result: &mut IndexResult,
        metadata_updates: &mut Vec<(String, i64, i64)>,
    ) -> Vec<Candidate> {
        let read_results: Vec<_> = entries
            .par_iter()
            .map(|entry| {
                match hasher::file_hash(&entry.abs_path) {
                    Ok((content, hash, is_binary)) => {
                        // Двоичный контент под видом code-файла (EDT-защищённые
                        // модули поставщика — .bsl с двоичным образом) переводим
                        // в Binary, чтобы не отдавать в tree-sitter.
                        let category = if is_binary {
                            FileCategory::Binary
                        } else {
                            entry.category.clone()
                        };
                        Ok((entry.rel_path.clone(), content, hash, category, entry.mtime, entry.file_size))
                    }
                    Err(e) => Err((entry.rel_path.clone(), e.to_string())),
                }
            })
            .collect();

        let mut candidates = Vec::new();
        for item in read_results {
            match item {
                Ok((rel_path, content, hash, category, mtime, file_size)) => {
                    // Двоичные файлы (в т.ч. распознанные по контенту в file_hash)
                    // в индекс не идут — ни парсинга, ни записи.
                    if matches!(category, FileCategory::Binary) {
                        result.files_not_indexable += 1;
                        continue;
                    }
                    if !force {
                        if let Some((_, existing_hash, _, _)) = existing_files.get(&rel_path) {
                            if *existing_hash == hash {
                                // Содержимое не изменилось, но mtime/size мог — обновим метаданные
                                metadata_updates.push((rel_path, mtime, file_size));
                                result.files_skipped += 1;
                                continue;
                            }
                        }
                    }
                    candidates.push((rel_path, content, hash, category, mtime, file_size));
                }
                Err((rel_path, error)) => {
                    result.errors.push((rel_path, error));
                }
            }
        }

        candidates
    }
}

/// Один файл, найденный обходом дерева: путь и сведения о нём. Содержимое ещё
/// не прочитано — оно читается порциями, уже после того, как отсеяны файлы с
/// неизменившимися временем и размером.
struct FileEntry {
    abs_path: std::path::PathBuf,
    rel_path: String,
    category: FileCategory,
    mtime: i64,
    file_size: i64,
}

/// Кандидат на индексацию: путь, содержимое, хеш, категория, время и размер.
type Candidate = (String, String, String, FileCategory, i64, i64);

/// Как порция пишется в базу — решения, принятые один раз на весь проход.
struct ChunkPolicy<'p> {
    /// Пропускать построчное удаление прежних строк файла: в пакетном режиме
    /// они убраны одним проходом заранее, при первичной индексации их нет вовсе.
    skip_delete: bool,
    /// Файлы, которые в базе уже есть. Задаётся только при продолжении
    /// прерванной загрузки: пакетного удаления там не было, поэтому те немногие
    /// файлы, что успели записаться до обрыва, чистят свои строки сами.
    existing_files: Option<&'p HashMap<String, (i64, String, Option<i64>, Option<i64>)>>,
    /// Номер порции и общее их число — только для журнала.
    label: Option<(usize, usize)>,
}

impl ChunkPolicy<'_> {
    /// Пропускать ли удаление прежних строк конкретного файла.
    fn skip_delete_for(&self, rel_path: &str) -> bool {
        match self.existing_files {
            Some(existing) => self.skip_delete && !existing.contains_key(rel_path),
            None => self.skip_delete,
        }
    }
}

/// Нарезать список файлов на порции так, чтобы вес исходников в каждой не
/// превышал бюджет.
///
/// Файл тяжелее бюджета идёт отдельной порцией — резать его нечем. Нулевой
/// бюджет означает «без деления»: весь список одной порцией.
fn chunk_by_budget<'e>(entries: &'e [&'e FileEntry], budget: usize) -> Vec<&'e [&'e FileEntry]> {
    if entries.is_empty() {
        return Vec::new();
    }
    if budget == 0 {
        return vec![entries];
    }
    let budget = budget as u64;
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut acc: u64 = 0;
    for (i, entry) in entries.iter().enumerate() {
        let size = entry.file_size.max(0) as u64;
        if i > start && acc + size > budget {
            chunks.push(&entries[start..i]);
            start = i;
            acc = 0;
        }
        acc += size;
    }
    if start < entries.len() {
        chunks.push(&entries[start..]);
    }
    chunks
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


    /// Нарезка на порции: файлы копятся, пока не исчерпан бюджет; файл тяжелее
    /// бюджета уходит в порцию один; нулевой бюджет отключает деление.
    #[test]
    fn test_chunk_by_budget() {
        let mk = |name: &str, size: i64| FileEntry {
            abs_path: std::path::PathBuf::from(name),
            rel_path: name.to_string(),
            category: FileCategory::Text,
            mtime: 0,
            file_size: size,
        };
        let entries = vec![mk("a", 40), mk("b", 40), mk("c", 40), mk("d", 500)];
        let refs: Vec<&FileEntry> = entries.iter().collect();

        let chunks = chunk_by_budget(&refs, 100);
        assert_eq!(chunks.len(), 3, "40+40 | 40 | 500 — три порции");
        assert_eq!(chunks[0].len(), 2);
        assert_eq!(chunks[1].len(), 1);
        assert_eq!(chunks[2].len(), 1, "файл тяжелее бюджета идёт порцией в одиночку");

        assert_eq!(chunk_by_budget(&refs, 0).len(), 1, "нулевой бюджет — без деления");
        assert!(chunk_by_budget(&[], 100).is_empty(), "пустой список — ни одной порции");
    }

    /// Порционный разбор даёт ровно тот же результат, что и разбор одним куском:
    /// бюджет режет работу, а не данные.
    #[test]
    fn test_chunked_initial_index_matches_whole() {
        let tmp = TempDir::new().unwrap();
        for i in 0..12 {
            fs::write(
                tmp.path().join(format!("mod{}.py", i)),
                format!("def f{}():\n    return {}\n", i, i),
            )
            .unwrap();
        }

        // Одним куском.
        let mut whole_storage = Storage::open_in_memory().unwrap();
        let whole = {
            let mut indexer = Indexer::with_config(
                &mut whole_storage,
                IndexConfig { chunk_budget_bytes: 0, ..IndexConfig::default() },
            );
            indexer.full_reindex(tmp.path(), false).unwrap()
        };

        // Порциями: бюджета в 30 байт хватает на один-два таких файла.
        let mut chunked_storage = Storage::open_in_memory().unwrap();
        let chunked = {
            let mut indexer = Indexer::with_config(
                &mut chunked_storage,
                IndexConfig { chunk_budget_bytes: 30, ..IndexConfig::default() },
            );
            indexer.full_reindex(tmp.path(), false).unwrap()
        };

        assert_eq!(chunked.errors.len(), 0, "ошибок быть не должно");
        assert_eq!(chunked.files_indexed, whole.files_indexed, "записано столько же файлов");

        let whole_stats = whole_storage.get_stats().unwrap();
        let chunked_stats = chunked_storage.get_stats().unwrap();
        assert_eq!(chunked_stats.total_files, 12);
        assert_eq!(chunked_stats.total_files, whole_stats.total_files);
        assert_eq!(chunked_stats.total_functions, whole_stats.total_functions);

        // Пакетный режим отработал до конца: отметка о незавершённой загрузке снята.
        assert!(!chunked_storage.bulk_in_progress(), "отметка должна быть снята");
    }

    /// Продолжение прерванной загрузки: отметка осталась с прошлого раза, часть
    /// файлов в базе уже есть. Проход дочитывает остаток, достраивает индексы и
    /// снимает отметку.
    #[test]
    fn test_resume_after_interrupted_bulk() {
        let tmp = TempDir::new().unwrap();
        for i in 0..3 {
            fs::write(
                tmp.path().join(format!("a{}.py", i)),
                format!("def f{}():\n    pass\n", i),
            )
            .unwrap();
        }

        let mut storage = Storage::open_in_memory().unwrap();
        {
            let mut indexer = Indexer::new(&mut storage);
            indexer.full_reindex(tmp.path(), false).unwrap();
        }

        // Изображаем обрыв: отметка стоит, индексы сняты, часть файлов не дописана.
        storage.set_bulk_in_progress(true).unwrap();
        storage.prepare_bulk_load().unwrap();
        for i in 3..6 {
            fs::write(
                tmp.path().join(format!("a{}.py", i)),
                format!("def f{}():\n    pass\n", i),
            )
            .unwrap();
        }

        let result = {
            let mut indexer = Indexer::new(&mut storage);
            indexer.full_reindex(tmp.path(), false).unwrap()
        };

        assert_eq!(result.files_indexed, 3, "дочитан только остаток");
        assert!(!storage.bulk_in_progress(), "отметка снята");

        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_files, 6);
        assert_eq!(stats.total_functions, 6);

        // Индексы достроены — иначе база осталась бы пригодной только для полного
        // скана, а поиск молча стал бы медленным.
        let index_count: i64 = storage
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_files_path'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 1, "индекс по пути файла должен быть на месте");
    }

    /// При докатке файл, записанный до обрыва и с тех пор изменившийся,
    /// переписывается без дублей: пакетного удаления в таком проходе не было,
    /// поэтому прежние строки он чистит сам.
    #[test]
    fn test_resume_rewrites_changed_file_without_duplicates() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("one.py");
        fs::write(&file, "def было():\n    pass\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        {
            let mut indexer = Indexer::new(&mut storage);
            indexer.full_reindex(tmp.path(), false).unwrap();
        }

        storage.set_bulk_in_progress(true).unwrap();
        // Другая длина содержимого — файл не отсеется сверкой времени и размера
        // даже в пределах одной секунды.
        fs::write(&file, "def стало_другим_именем():\n    return 42\n").unwrap();

        {
            let mut indexer = Indexer::new(&mut storage);
            indexer.full_reindex(tmp.path(), false).unwrap();
        }

        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_files, 1);
        assert_eq!(stats.total_functions, 1, "прежняя функция не должна остаться рядом с новой");
    }

    /// S-6: списки путей нужны старту демона, чтобы звать точечный пересбор
    /// надстройки вместо полного. Проверяем, что пути реально собираются —
    /// и записанные, и удалённые.
    #[test]
    fn test_reindex_collects_changed_and_deleted_paths() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.py"), "def a():\n    pass\n").unwrap();
        fs::write(tmp.path().join("b.py"), "def b():\n    pass\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let first = {
            let mut indexer = Indexer::new(&mut storage);
            indexer.full_reindex(tmp.path(), false).unwrap()
        };
        assert!(!first.paths_overflow, "два файла в потолок не упираются");
        let mut got: Vec<&str> = first.changed_paths.iter().map(|s| s.as_str()).collect();
        got.sort();
        assert_eq!(got, vec!["a.py", "b.py"], "оба файла должны попасть в changed_paths");
        assert!(first.deleted_paths.is_empty(), "удалять нечего");

        // Один файл меняем, другой убираем — второй проход должен показать оба
        // события, а неизменённых файлов в списке быть не должно.
        fs::write(tmp.path().join("a.py"), "def a():\n    return 1\n").unwrap();
        fs::remove_file(tmp.path().join("b.py")).unwrap();
        let second = {
            let mut indexer = Indexer::new(&mut storage);
            indexer.full_reindex(tmp.path(), false).unwrap()
        };
        assert_eq!(second.changed_paths, vec!["a.py"], "изменён только a.py");
        assert_eq!(second.deleted_paths, vec!["b.py"], "удалён только b.py");
    }

    /// S-6: за потолком точечный пересбор дороже полного, поэтому списки
    /// очищаются и взводится признак переполнения — вызывающий идёт полным путём.
    #[test]
    fn test_paths_overflow_clears_lists() {
        let mut r = IndexResult {
            files_scanned: 0,
            files_indexed: 0,
            files_skipped: 0,
            files_not_indexable: 0,
            files_deleted: 0,
            errors: vec![],
            elapsed_ms: 0,
            changed_paths: Vec::new(),
            deleted_paths: Vec::new(),
            paths_overflow: false,
        };
        for i in 0..PATHS_CAP {
            r.note_changed(&format!("f{i}.py"));
        }
        assert!(!r.paths_overflow, "ровно потолок — ещё не переполнение");
        assert_eq!(r.changed_paths.len(), PATHS_CAP);

        r.note_changed("one_more.py");
        assert!(r.paths_overflow, "шаг за потолок взводит признак");
        assert!(r.changed_paths.is_empty(), "списки очищены, память не копится");
        assert!(r.deleted_paths.is_empty());

        // После переполнения накопление прекращается — иначе память всё равно росла бы.
        r.note_changed("and_another.py");
        r.note_deleted("gone.py");
        assert!(r.changed_paths.is_empty());
        assert!(r.deleted_paths.is_empty());
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

    #[test]
    fn test_with_config_custom_exclude() {
        let tmp = TempDir::new().unwrap();
        // Создаём директорию vendor с файлом
        fs::create_dir(tmp.path().join("vendor")).unwrap();
        fs::write(tmp.path().join("vendor").join("lib.py"), "x = 1\n").unwrap();
        // Основной файл проекта
        fs::write(tmp.path().join("app.py"), "y = 2\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let config = IndexConfig {
            exclude_dirs: vec!["vendor".to_string()],
            ..Default::default()
        };
        let mut indexer = Indexer::with_config(&mut storage, config);
        let r = indexer.full_reindex(tmp.path(), false).unwrap();

        // vendor/ исключён через конфиг — только app.py
        assert_eq!(r.files_indexed, 1, "vendor должен быть исключён через конфиг");
    }

    #[test]
    fn test_bulk_load_mode() {
        let tmp = TempDir::new().unwrap();

        // Создаём 15 Python-файлов с уникальными функциями
        for i in 0..15 {
            fs::write(
                tmp.path().join(format!("module_{i}.py")),
                format!(
                    "def func_{i}(x):\n    \"\"\"Функция номер {i}.\"\"\"\n    return x + {i}\n"
                ),
            )
            .unwrap();
        }

        let mut storage = Storage::open_in_memory().unwrap();

        // Устанавливаем порог 10 — при 15 файлах должен включиться bulk-load
        let config = IndexConfig {
            bulk_threshold: 10,
            ..Default::default()
        };

        // Первый проход: индексируем все 15 файлов в bulk-load режиме
        {
            let mut indexer = Indexer::with_config(&mut storage, config.clone());
            let result = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(result.files_indexed, 15, "все 15 файлов должны быть проиндексированы");
            assert_eq!(result.files_skipped, 0, "пропущенных файлов быть не должно");
            assert_eq!(result.errors.len(), 0, "ошибок быть не должно");
        }

        // Проверяем статистику в БД (indexer уже дропнут)
        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_files, 15, "в БД должно быть 15 файлов");
        assert_eq!(stats.total_functions, 15, "по одной функции на файл");

        // Проверяем, что FTS работает после rebuild
        let found = storage.search_functions("func_0", 10, None).unwrap();
        assert!(!found.is_empty(), "FTS должен находить func_0 после bulk-load rebuild");

        let found_5 = storage.search_functions("func_5", 10, None).unwrap();
        assert!(!found_5.is_empty(), "FTS должен находить func_5 после bulk-load rebuild");

        // Второй проход: повторная индексация — все файлы должны быть пропущены
        {
            let mut indexer = Indexer::with_config(&mut storage, config);
            let result2 = indexer.full_reindex(tmp.path(), false).unwrap();
            assert_eq!(result2.files_skipped, 15, "при повторной индексации все файлы неизменны");
            assert_eq!(result2.files_indexed, 0, "ни одного файла не должно быть переиндексировано");
        }
    }

    #[test]
    fn test_with_config_max_file_size() {
        let tmp = TempDir::new().unwrap();
        // Маленький текстовый файл — пройдёт
        fs::write(tmp.path().join("small.txt"), "x = 1\n").unwrap();
        // Большой текстовый файл — пропустим (лимит 10 байт)
        // Лимит max_file_size действует только на Text-файлы, код индексируется всегда
        fs::write(tmp.path().join("big.txt"), "y = 'a very long string that exceeds limit'\n").unwrap();
        // Большой код-файл — НЕ пропускается (код индексируется независимо от размера)
        fs::write(tmp.path().join("big.py"), "y = 'a very long string that exceeds limit'\n").unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let config = IndexConfig {
            max_file_size: 10, // 10 байт
            ..Default::default()
        };
        let mut indexer = Indexer::with_config(&mut storage, config);
        let r = indexer.full_reindex(tmp.path(), false).unwrap();

        // big.txt пропущен из-за лимита размера, big.py — нет (код не ограничен)
        assert_eq!(r.files_indexed, 2, "small.txt + big.py (код не ограничен размером)");
        assert_eq!(
            r.files_not_indexable, 1,
            "big.txt не индексируется по размеру — это отдельный счётчик, не «без изменений»"
        );
        assert_eq!(r.files_skipped, 0, "неизменившихся файлов здесь нет");
    }

    /// Файлы выгрузки 1С, по которым реально ищут (оглавление конфигурации,
    /// права роли, состав формы), индексируются независимо от лимита: в крупных
    /// конфигурациях они перерастают мегабайт и выпадали из индекса целиком.
    /// Макеты печатных форм под исключение не подпадают — они бывают по 78 МБ.
    #[test]
    fn test_1c_dump_files_ignore_size_limit() {
        let tmp = TempDir::new().unwrap();
        let big = "<Metadata>".to_string() + &"x".repeat(500) + "</Metadata>\n";

        fs::write(tmp.path().join("Configuration.xml"), &big).unwrap();
        fs::write(tmp.path().join("Rights.xml"), &big).unwrap();
        fs::write(tmp.path().join("Form.xml"), &big).unwrap();
        // Не в исключении: макет печатной формы.
        fs::write(tmp.path().join("Template.xml"), &big).unwrap();
        // Служебная опись выгрузки исключена ещё раньше — как двоичная
        // (categorize_file), поэтому в счётчик пропущенных по размеру не идёт.
        fs::write(tmp.path().join("ConfigDumpInfo.xml"), &big).unwrap();

        let mut storage = Storage::open_in_memory().unwrap();
        let config = IndexConfig {
            max_file_size: 10, // 10 байт — все файлы заведомо крупнее
            ..Default::default()
        };
        let mut indexer = Indexer::with_config(&mut storage, config);
        let r = indexer.full_reindex(tmp.path(), false).unwrap();

        assert_eq!(
            r.files_indexed, 3,
            "Configuration.xml, Rights.xml и Form.xml должны индексироваться вопреки лимиту"
        );
        assert_eq!(
            r.files_not_indexable, 1,
            "Template.xml остаётся под лимитом (опись выгрузки отсеяна как двоичная)"
        );
    }

    #[test]
    fn test_batch_transactions() {
        let tmp = TempDir::new().unwrap();

        // Создаём 20 Python-файлов с уникальными функциями
        for i in 0..20 {
            fs::write(
                tmp.path().join(format!("module_{i}.py")),
                format!(
                    "def batch_func_{i}(x):\n    \"\"\"Функция батча {i}.\"\"\"\n    return x * {i}\n"
                ),
            )
            .unwrap();
        }

        let mut storage = Storage::open_in_memory().unwrap();

        // Устанавливаем маленький batch_size = 5, чтобы проверить несколько коммитов
        let config = IndexConfig {
            batch_size: 5,
            bulk_threshold: 100, // отключаем bulk-mode, чтобы проверять именно батч-транзакции
            ..Default::default()
        };

        let result = {
            let mut indexer = Indexer::with_config(&mut storage, config);
            indexer.full_reindex(tmp.path(), false).unwrap()
        };

        // Все 20 файлов должны быть успешно проиндексированы
        assert_eq!(result.files_indexed, 20, "все 20 файлов должны быть проиндексированы");
        assert_eq!(result.files_skipped, 0, "пропущенных файлов быть не должно");
        assert_eq!(result.errors.len(), 0, "ошибок быть не должно");

        // Данные реально записаны в БД — проверяем через get_stats
        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_files, 20, "в БД должно быть 20 файлов");
        assert_eq!(stats.total_functions, 20, "по одной функции на файл");

        // FTS должен находить функции
        let found = storage.search_functions("batch_func_0", 10, None).unwrap();
        assert!(!found.is_empty(), "FTS должен находить batch_func_0");

        let found_19 = storage.search_functions("batch_func_19", 10, None).unwrap();
        assert!(!found_19.is_empty(), "FTS должен находить batch_func_19 (последний батч)");
    }

    #[test]
    fn test_parallel_reindex() {
        let tmp = TempDir::new().unwrap();

        // Создаём 30 Python-файлов с разными функциями
        for i in 0..30 {
            fs::write(
                tmp.path().join(format!("parallel_{i}.py")),
                format!(
                    "def parallel_func_{i}(a, b):\n    \"\"\"Параллельная функция {i}.\"\"\"\n    return a + b + {i}\n\ndef helper_{i}(x):\n    return x * {i}\n"
                ),
            )
            .unwrap();
        }

        let mut storage = Storage::open_in_memory().unwrap();
        let mut indexer = Indexer::new(&mut storage);
        let result = indexer.full_reindex(tmp.path(), false).unwrap();

        // Все 30 файлов проиндексированы
        assert_eq!(result.files_indexed, 30, "все 30 файлов должны быть проиндексированы");
        assert_eq!(result.files_skipped, 0, "пропущенных файлов быть не должно");
        assert_eq!(result.errors.len(), 0, "ошибок при параллельном парсинге быть не должно");

        // Проверяем что все функции на месте (по 2 на файл = 60 итого)
        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_files, 30, "в БД должно быть 30 файлов");
        assert_eq!(stats.total_functions, 60, "по 2 функции на файл = 60 итого");

        // FTS находит функции из разных файлов (порядок парсинга не важен)
        let found_0 = storage.search_functions("parallel_func_0", 10, None).unwrap();
        assert!(!found_0.is_empty(), "FTS должен находить parallel_func_0");

        let found_15 = storage.search_functions("parallel_func_15", 10, None).unwrap();
        assert!(!found_15.is_empty(), "FTS должен находить parallel_func_15");

        let found_29 = storage.search_functions("parallel_func_29", 10, None).unwrap();
        assert!(!found_29.is_empty(), "FTS должен находить parallel_func_29");

        // helper-функции тоже проиндексированы
        let found_helper = storage.search_functions("helper_0", 10, None).unwrap();
        assert!(!found_helper.is_empty(), "FTS должен находить helper_0");
    }

    /// Тест: первичная индексация пустой БД в bulk-режиме.
    ///
    /// Проверяет, что при is_fresh_db=true + bulk_mode=true:
    /// - все файлы проиндексированы корректно
    /// - FTS-поиск работает после rebuild индексов
    /// - повторная индексация пропускает все неизменённые файлы
    #[test]
    fn test_bulk_fresh_db() {
        let tmp = TempDir::new().unwrap();

        // Создаём 20 Python-файлов с уникальными функциями
        for i in 0..20 {
            fs::write(
                tmp.path().join(format!("fresh_{i}.py")),
                format!(
                    "def fresh_func_{i}(x):\n    \"\"\"Свежая функция {i}.\"\"\"\n    return x + {i}\n"
                ),
            )
            .unwrap();
        }

        // Порог bulk_threshold=5 — при 20 файлах гарантированно активируется bulk-режим
        let config = IndexConfig {
            bulk_threshold: 5,
            ..Default::default()
        };

        let mut storage = Storage::open_in_memory().unwrap();

        // Первичная индексация пустой БД (is_fresh_db = true)
        let result = {
            let mut indexer = Indexer::with_config(&mut storage, config.clone());
            indexer.full_reindex(tmp.path(), false).unwrap()
        };

        assert_eq!(result.files_indexed, 20, "все 20 файлов должны быть проиндексированы");
        assert_eq!(result.files_skipped, 0, "пропущенных файлов быть не должно");
        assert_eq!(result.errors.len(), 0, "ошибок быть не должно");

        // Проверяем статистику
        let stats = storage.get_stats().unwrap();
        assert_eq!(stats.total_files, 20, "в БД должно быть 20 файлов");
        assert_eq!(stats.total_functions, 20, "по одной функции на файл");

        // Проверяем FTS-поиск после bulk rebuild
        let found_0 = storage.search_functions("fresh_func_0", 10, None).unwrap();
        assert!(!found_0.is_empty(), "FTS должен находить fresh_func_0 после bulk-load rebuild");

        let found_19 = storage.search_functions("fresh_func_19", 10, None).unwrap();
        assert!(!found_19.is_empty(), "FTS должен находить fresh_func_19 после bulk-load rebuild");

        // Повторная индексация (is_fresh_db = false) — все файлы должны быть пропущены
        let result2 = {
            let mut indexer = Indexer::with_config(&mut storage, config);
            indexer.full_reindex(tmp.path(), false).unwrap()
        };

        assert_eq!(result2.files_skipped, 20, "при повторной индексации все 20 файлов неизменны");
        assert_eq!(result2.files_indexed, 0, "ни одного файла не должно быть переиндексировано");

        // FTS по-прежнему работает после повторного прохода
        let found_after = storage.search_functions("fresh_func_10", 10, None).unwrap();
        assert!(!found_after.is_empty(), "FTS должен работать и после повторной индексации");
    }

    /// Тест: bulk-ОБНОВЛЕНИЕ непустой БД — сценарий бага квадратичной деградации.
    ///
    /// Индексируем набор файлов, затем меняем содержимое ВСЕХ (> bulk_threshold)
    /// и переиндексируем. Проверяем, что после bulk-обновления:
    /// - нет дублей строк (functions/classes/imports/calls/variables/text) —
    ///   счётчики строго совпадают со свежей индексацией того же финального среза;
    /// - FTS находит НОВЫЕ символы и НЕ находит старые (rebuild functions/classes
    ///   + отсутствие висячих токенов contentless-указателя fts_text_files).
    #[test]
    fn test_bulk_update_existing_db() {
        // Пишет исходный (upd=false) или финальный (upd=true) вариант всех файлов.
        fn write_variant(dir: &std::path::Path, upd: bool) {
            for i in 0..20 {
                let code = if upd {
                    format!("def upd_func_{i}(x):\n    \"\"\"Обновлённая {i}.\"\"\"\n    other_{i}(x)\n    z = x * {i}\n    return z\n")
                } else {
                    format!("def orig_func_{i}(x):\n    \"\"\"Оригинальная {i}.\"\"\"\n    helper_{i}(x)\n    y = x + {i}\n    return y\n")
                };
                fs::write(dir.join(format!("mod_{i}.py")), code).unwrap();
            }
            // Текстовые файлы (проверяют путь text_contents + contentless FTS).
            // Маркеры РАЗНОЙ длины — чтобы менялся размер файла и mtime+size
            // fast-path не счёл текст неизменным (иначе .md были бы пропущены).
            for i in 0..6 {
                let marker = if upd { "zzfreshtoken" } else { "zzoldmarker" };
                fs::write(
                    dir.join(format!("note_{i}.md")),
                    format!("# Заметка {i}\n{marker} строка {i}\n"),
                )
                .unwrap();
            }
        }

        // 26 файлов >> порога 5 → гарантированно bulk-режим.
        let config = IndexConfig {
            bulk_threshold: 5,
            ..Default::default()
        };

        // ── Эталон: свежая индексация ФИНАЛЬНОГО среза в отдельную БД ─────────
        let ref_dir = TempDir::new().unwrap();
        write_variant(ref_dir.path(), true);
        let mut ref_storage = Storage::open_in_memory().unwrap();
        {
            let mut indexer = Indexer::with_config(&mut ref_storage, config.clone());
            indexer.full_reindex(ref_dir.path(), false).unwrap();
        }
        let baseline = ref_storage.get_stats().unwrap();

        // ── Проверяемый путь: индексируем ORIG, затем перезаписываем на UPD ───
        let upd_dir = TempDir::new().unwrap();
        write_variant(upd_dir.path(), false);
        let mut storage = Storage::open_in_memory().unwrap();
        {
            let mut indexer = Indexer::with_config(&mut storage, config.clone());
            indexer.full_reindex(upd_dir.path(), false).unwrap();
        }
        // Меняем содержимое ВСЕХ файлов → все становятся кандидатами (> порога),
        // is_fresh_db=false → ветка bulk-ОБНОВЛЕНИЯ непустой БД.
        write_variant(upd_dir.path(), true);
        let result = {
            let mut indexer = Indexer::with_config(&mut storage, config);
            indexer.full_reindex(upd_dir.path(), false).unwrap()
        };
        assert_eq!(result.files_skipped, 0, "все файлы изменены — пропусков нет");
        assert_eq!(result.errors.len(), 0, "ошибок быть не должно");

        // ── Нет дублей: счётчики строго равны свежей индексации ──────────────
        let after = storage.get_stats().unwrap();
        assert_eq!(after.total_files, baseline.total_files, "files: без дублей");
        assert_eq!(after.total_functions, baseline.total_functions, "functions: без дублей");
        assert_eq!(after.total_classes, baseline.total_classes, "classes: без дублей");
        assert_eq!(after.total_imports, baseline.total_imports, "imports: без дублей");
        assert_eq!(after.total_calls, baseline.total_calls, "calls: без дублей");
        assert_eq!(after.total_variables, baseline.total_variables, "variables: без дублей");
        assert_eq!(after.total_text_files, baseline.total_text_files, "text: без дублей");

        // ── FTS: новые символы находятся, старые — нет ───────────────────────
        assert!(
            !storage.search_functions("upd_func_0", 10, None).unwrap().is_empty(),
            "FTS должен находить обновлённую функцию после rebuild"
        );
        assert!(
            storage.search_functions("orig_func_0", 10, None).unwrap().is_empty(),
            "старая функция не должна оставаться в FTS после bulk-обновления"
        );
        assert!(
            !storage.search_text("zzfreshtoken", 10, None).unwrap().is_empty(),
            "текстовый FTS должен находить новый маркер"
        );
        assert!(
            storage.search_text("zzoldmarker", 10, None).unwrap().is_empty(),
            "старый текстовый маркер не должен оставаться в contentless-указателе"
        );
    }
}
