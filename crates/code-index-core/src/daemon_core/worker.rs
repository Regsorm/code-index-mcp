// Worker одной отслеживаемой папки. Делает initial reindex + держит watcher-цикл.
//
// Работа полностью блокирующая (tree-sitter, rayon, notify), поэтому worker
// запускается из runner'а через `tokio::task::spawn_blocking`. Взаимодействие с
// tokio-миром только через `DaemonState` (асинхронный RwLock) и через
// `shutdown_rx` (broadcast).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::extension::{LanguageProcessor, ProcessorRegistry};
use crate::indexer::config::IndexConfig;
use crate::indexer::file_types::{categorize_file_in_repo, FileCategory};
use crate::indexer::hasher;
use crate::indexer::Indexer;
use crate::parser::text::TextParser;
use crate::parser::LanguageParser;
use crate::parser::ParserRegistry;
use crate::storage::memory::StorageConfig;
use crate::storage::Storage;
use crate::watcher::{create_watcher, poll_batch, FileEvent, WatcherConfig};

use super::cache_client::CacheClient;
use super::config::{IndexerSection, PathEntry};
use super::ipc::{PathStatus, Progress};
use super::state::DaemonState;

/// Название режима хранилища для журнала. Пользователю, приславшему журнал,
/// важно видеть, работала база в памяти или на диске: от этого зависят и
/// расход памяти, и поведение при обрыве.
fn storage_mode_ru(mode: &crate::storage::memory::StorageMode) -> &'static str {
    match mode {
        crate::storage::memory::StorageMode::InMemory => "в оперативной памяти",
        crate::storage::memory::StorageMode::Disk => "на диске",
    }
}

/// Есть ли в базе хоть одна запись о файле.
///
/// По наличию файла судить нельзя: сервер выдачи при старте создаёт пустые
/// базы для всех местных папок, чтобы открыть их только на чтение до первой
/// индексации. Такая пустышка от настоящей базы по имени и размеру не
/// отличается, и демон принимал первичную индексацию за сверку изменений —
/// врал в журнале и открывал базу сразу на диске вместо памяти.
fn db_has_data(db_path: &std::path::Path) -> bool {
    if !db_path.exists() {
        return false;
    }
    let Ok(storage) = Storage::open_file_readonly(db_path) else {
        // Файл нечитаем или это ещё не база — данных в нём для нас нет.
        return false;
    };
    storage
        .conn()
        .query_row("SELECT EXISTS(SELECT 1 FROM files)", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|exists| exists != 0)
        .unwrap_or(false)
}

/// Что делать циклу слежения после обработки пакета.
enum BatchStep {
    /// Пакет отработан (успешно или с отмеченной ошибкой) — ждём следующий.
    Continue,
    /// Дальше работать нельзя: транзакцию начать не удалось. Воркер выходит,
    /// сторож в runner поднимет его со свежим соединением.
    Stop,
}

/// Неизменная обвязка воркера: всё, что нужно обработке пакета, кроме самого
/// пакета и хранилища. Ссылки, а не владение — цикл слежения владеет этими
/// значениями и переиспользует их от пакета к пакету.
struct BatchContext<'a> {
    path: &'a PathBuf,
    entry: &'a PathEntry,
    state: &'a DaemonState,
    registry: &'a ParserRegistry,
    max_code_file_size: usize,
    repo_language: Option<&'a str>,
    /// Дополнительные текстовые расширения из настроек проекта.
    extra_text_extensions: &'a [String],
    resolved_processor: Option<&'a Arc<dyn LanguageProcessor>>,
    cache_client: Option<&'a Arc<CacheClient>>,
    /// Настройки индексации папки — нужны, когда пачка велика и её выгоднее
    /// обработать полным проходом, а не по одному файлу.
    index_config: &'a IndexConfig,
}

/// Обработать один пакет событий файловой системы: пометить пути «грязными» в
/// кэше выдачи, применить каждое событие, зафиксировать транзакцию, пересобрать
/// надстройку по изменившимся файлам, сбросить кэш и выставить итоговый статус.
///
/// Вынесено из `run_worker` (находка G-2): цикл слежения теперь читается как
/// «взять пакет → обработать → решить, продолжать ли», а сама обработка стала
/// самостоятельной единицей.
fn process_batch(
    ctx: &BatchContext<'_>,
    storage: &mut Storage,
    batch: &[FileEvent],
) -> BatchStep {
    // Ранний mark-dirty (#1471): сообщаем cache-ci об изменённых путях с
    // observed-mtime ДО переразбора/commit. Это даёт прокси сразу пометить
    // зависимые записи «грязными» и не отдавать их как HIT, пока индекс не
    // догнал диск. В дополнение к invalidate после commit (ниже); снимается
    // сверкой mtime на стороне cache-ci. Best-effort.
    if let Some(cc) = ctx.cache_client {
        if !cc.is_empty() {
            let dirty = collect_dirty_paths(ctx.path, &batch);
            if !dirty.is_empty() {
                let cc_clone = cc.clone();
                let repo = ctx.entry.effective_alias();
                tokio_block_on(async move {
                    cc_clone.mark_dirty_files(&repo, &dirty).await;
                });
            }
        }
    }

    let batch_started = std::time::Instant::now();
    let batch_started_local = crate::logging::local_hms();
    crate::logging::stages_reset();
    // Сколько заняла надстройка целиком — для раскладки в итоге, как у полной
    // индексации. Ноль означает, что её не трогали.
    let mut extras_ms: u128 = 0;

    tokio_block_on(async {
        ctx.state.set_status(ctx.path, PathStatus::ReindexingBatch).await;
        ctx.state
            .set_progress(ctx.path, Progress::new(0, batch.len()))
            .await;
    });

    let batch_len = batch.len();
    // Каким двигателем обрабатывать пачку. Пофайловый путь идёт в один поток и
    // пишет при живых индексах — на файл он в разы дороже полного прохода,
    // который читает и разбирает параллельно, а вставляет пакетно, сняв
    // индексы. На правке нескольких файлов дешевле первый, на массовом
    // обновлении — второй. Замер на типовой торговой конфигурации: пачка в
    // 3 437 файлов пофайлово — 2 мин 50 с, полный проход по всем 57 072 —
    // 1 мин 51 с.
    if batch_len > ctx.index_config.bulk_batch_threshold {
        return process_batch_full_pass(ctx, storage, batch, batch_started, batch_started_local);
    }

    // Начало обработки отмечаем прямо: сколько файлов и каким путём идём.
    // Иначе выбор пути виден лишь по косвенным признакам, а между строкой о
    // числе файлов и первым отчётом надстройки — полминуты тишины. Черту
    // здесь не ставим: она уже стоит перед строкой о собранных изменениях.
    tracing::info!(
        "[{}] начата частичная индексация: изменившихся файлов {} — обрабатываю пофайлово \
         (порог перехода на пакетную обработку: {} файлов)",
        ctx.path.display(),
        batch_len,
        ctx.index_config.bulk_batch_threshold
    );

    if let Err(e) = storage.begin_batch() {
        tracing::error!("[{}] не удалось начать транзакцию пакета: {}", ctx.path.display(), e);
        // Транзакцию начать не удалось — данные батча НЕ применены. Не выдаём
        // Ready (был бы ложный «готово» на старом срезе). Помечаем Error и
        // выходим из воркера: сторож в runner перезапустит его со свежим
        // соединением (лечит и возможное залипание открытой транзакции).
        tokio_block_on(async {
            ctx.state
                .set_error(
                    ctx.path,
                    "не удалось начать транзакцию батча — воркер перезапускается",
                )
                .await;
        });
        return BatchStep::Stop;
    }

    let apply_started = std::time::Instant::now();
    crate::logging::stage_begin("применение изменившихся файлов");
    let mut progress = crate::logging::Heartbeat::every_secs(5);
    let mut done = 0usize;
    // Сколько событий батча применить не удалось. Ненулевой счётчик означает
    // неполный срез: часть файлов осталась в индексе прежней версии.
    let mut failed = 0usize;
    for event in batch {
        if !apply_event(
            storage,
            ctx.path,
            event,
            ctx.registry,
            ctx.max_code_file_size,
            ctx.repo_language,
            ctx.extra_text_extensions,
        ) {
            failed += 1;
        }
        done += 1;
        if done % 50 == 0 || done == batch_len {
            tokio_block_on(async {
                ctx.state
                    .set_progress(ctx.path, Progress::new(done, batch_len))
                    .await;
            });
        }
        // Отчёт в журнал раз в несколько секунд: без него от начала обработки
        // до первой строки надстройки стоит тишина в десятки секунд, и по
        // журналу не отличить работу от остановки.
        if progress.due() {
            tracing::info!(
                "[{}] разобрано и записано {} из {} изменившихся файлов",
                ctx.path.display(),
                done,
                batch_len
            );
        }
    }

    // Этапы ядра пофайлово отмечает `apply_event` — те же имена, что при полной
    // индексации. Здесь остаётся проставить, сколько файлов прошло через них.
    let applied = crate::logging::plural(batch_len as u64, "файл", "файла", "файлов");
    let applied = if failed > 0 {
        format!("{}, сбоев {}", applied, failed)
    } else {
        applied
    };
    crate::logging::stage_set_detail("разбор файлов", applied.clone());
    crate::logging::stage_set_detail("запись в базу", applied);
    let _ = apply_started;

    // Файлы применены: снять отметку об этапе и обнулить счётчик. Иначе строка
    // состояния демона продолжает показывать «применение файлов, 2000/2000
    // (100 %)», пока идёт надстройка, — то есть врёт о том, чем папка занята.
    crate::logging::stage_begin("фиксация изменений");
    tokio_block_on(async {
        ctx.state.set_progress(ctx.path, Progress::new(0, 0)).await;
    });

    let commit_started = std::time::Instant::now();
    let commit_ok = match storage.commit_batch() {
        Ok(()) => true,
        Err(e) => {
            tracing::error!("[{}] не удалось зафиксировать пакет: {}", ctx.path.display(), e);
            // Фиксация не удалась — данных батча в базе НЕТ. Откатываем, чтобы
            // соединение не осталось с открытой транзакцией (SQLITE_BUSY на
            // COMMIT её не снимает) и следующий begin_batch не упал.
            if let Err(re) = storage.rollback_batch() {
                tracing::error!(
                    "[{}] откат после несостоявшейся фиксации не удался: {}",
                    ctx.path.display(),
                    re
                );
            }
            false
        }
    };

    // Фиксация транзакции — часть записи в базу, отдельным этапом её не выносим:
    // при полной индексации она тоже внутри «записи в базу».
    crate::logging::stage_add("запись в базу", commit_started.elapsed());
    crate::logging::stage_idle();

    // Пересобрался ли слой extras для этого батча. Провал означает, что тела
    // функций свежие, а граф вызовов и связи данных отстают — такой срез
    // нельзя объявлять готовым.
    let mut extras_ok = true;

    // 8a. Инкрементальное обновление extras-слоя (граф вызовов, data_links,
    //     структура объектов, формы, подписки) для файлов этого батча.
    //     Базовый индекс уже закоммичен (calls/AST/file_contents свежие),
    //     поэтому slice-rebuild графа из `calls` корректен. Для
    //     универсальных языков — no-op (default-impl trait'а). Функция ведёт
    //     свои BEGIN/COMMIT внутри, поэтому вызывается после commit_batch.
    if commit_ok {
        if let Some(proc) = ctx.resolved_processor {
            let mut changed_paths: Vec<PathBuf> = Vec::new();
            let mut deleted_paths: Vec<PathBuf> = Vec::new();
            for event in batch {
                match event {
                    FileEvent::Modified(p) | FileEvent::Created(p) => {
                        changed_paths.push(p.clone())
                    }
                    FileEvent::Deleted(p) => deleted_paths.push(p.clone()),
                }
            }
            let t0 = std::time::Instant::now();
            match proc.index_extras_for_files(
                ctx.path,
                storage,
                &changed_paths,
                &deleted_paths,
            ) {
                Ok(()) => tracing::info!(
                    "[{}] надстройка обновлена точечно за {} мс (изменено {}, удалено {})",
                    ctx.path.display(), t0.elapsed().as_millis(),
                    changed_paths.len(), deleted_paths.len()
                ),
                Err(e) => {
                    tracing::warn!(
                        "[{}] точечное обновление надстройки процессора «{}» упало: {}. \
                         Базовая индексация при этом сохранена.",
                        ctx.path.display(), proc.name(), e
                    );
                    extras_ok = false;
                }
            }
            extras_ms = t0.elapsed().as_millis();
        }
    }

    finish_batch(
        ctx,
        storage,
        batch,
        BatchResult {
            commit_ok,
            failed,
            batch_len,
            extras_ok,
            extras_ms,
            started: batch_started,
            started_local: batch_started_local,
        },
    )
}

/// Обработать большую пачку изменений полным проходом.
///
/// Пофайловый путь применяет события по одному, в один поток и при живых
/// индексах — на массовом обновлении это в разы дороже полного прохода, где
/// чтение, хеширование и разбор идут по всем ядрам, а вставка выполняется
/// пакетно со снятыми индексами. Отбор изменившихся полный проход делает сам
/// (по времени и размеру файла), поэтому список событий ему не нужен — он
/// остаётся только для сброса кэша выдачи.
///
/// Надстройка здесь тоже пересобирается целиком: на таком объёме точечное
/// обновление дороже (замер: 3 437 файлов точечно — 1 мин 59 с против 54 с
/// полного пересбора по всей конфигурации).
fn process_batch_full_pass(
    ctx: &BatchContext<'_>,
    storage: &mut Storage,
    batch: &[FileEvent],
    batch_started: std::time::Instant,
    batch_started_local: String,
) -> BatchStep {
    let batch_len = batch.len();
    tracing::info!(
        "[{}] начата частичная индексация: изменившихся файлов {} — обрабатываю пакетно \
         (порог перехода на пакетную обработку: {} файлов, превышен): обход всего дерева, \
         разбор в несколько потоков, запись пачками; переписываются только изменившиеся \
         файлы, надстройка пересобирается целиком",
        ctx.path.display(),
        batch_len,
        ctx.index_config.bulk_batch_threshold
    );

    // Сборщик надстройки участвует только в полном разборе — здесь он уместен
    // ровно так же, как при индексации на старте демона.
    let parse_collector = ctx.resolved_processor.and_then(|proc| proc.parse_collector());
    let core_ok = {
        let mut indexer = Indexer::with_config(storage, ctx.index_config.clone());
        match indexer.full_reindex_with_collector(ctx.path, false, parse_collector.as_deref()) {
            Ok(result) => {
                tracing::info!(
                    "[{}] пакетная обработка закончена: просмотрено {} файлов — записано {}, \
                     без изменений {}, не индексируется {}, удалено {}",
                    ctx.path.display(),
                    result.files_scanned,
                    result.files_indexed,
                    result.files_skipped,
                    result.files_not_indexable,
                    result.files_deleted
                );
                true
            }
            Err(e) => {
                tracing::error!(
                    "[{}] пакетная обработка пачки изменений не удалась: {}",
                    ctx.path.display(),
                    e
                );
                false
            }
        }
    };

    let mut extras_ok = true;
    let mut extras_ms: u128 = 0;
    if core_ok {
        if let Some(proc) = ctx.resolved_processor {
            let t0 = std::time::Instant::now();
            crate::logging::stage_begin("надстройка целиком");
            match proc.index_extras(ctx.path, storage) {
                Ok(()) => tracing::info!(
                    "[{}] надстройка пересобрана целиком за {} мс",
                    ctx.path.display(),
                    t0.elapsed().as_millis()
                ),
                Err(e) => {
                    tracing::warn!(
                        "[{}] полный пересбор надстройки процессора «{}» упал: {}. \
                         Базовая индексация при этом сохранена.",
                        ctx.path.display(),
                        proc.name(),
                        e
                    );
                    extras_ok = false;
                }
            }
            extras_ms = t0.elapsed().as_millis();
            // Отдельным этапом надстройку в раскладку не добавляем: её слои уже
            // перечислены выше, а общее время стоит в итоговой строке. Строка
            // «надстройка целиком» была суммой предыдущих и читалась как
            // двойной счёт.
            crate::logging::stage_idle();
        }
    }

    finish_batch(
        ctx,
        storage,
        batch,
        BatchResult {
            commit_ok: core_ok,
            // Полный проход не считает сбои по отдельным файлам: он либо
            // прошёл целиком, либо вернул ошибку.
            failed: 0,
            batch_len,
            extras_ok,
            extras_ms,
            started: batch_started,
            started_local: batch_started_local,
        },
    )
}

/// Итоги обработки пачки — всё, что нужно завершающему шагу.
struct BatchResult {
    commit_ok: bool,
    failed: usize,
    batch_len: usize,
    extras_ok: bool,
    extras_ms: u128,
    started: std::time::Instant,
    started_local: String,
}

/// Завершить обработку пачки: схлопнуть журнал WAL, сбросить кэш выдачи,
/// напечатать сводку и выставить статус папки.
///
/// Общий хвост для обоих путей — пофайлового и полного прохода: различаются
/// они только тем, как применяли изменения, а заканчиваются одинаково.
fn finish_batch(
    ctx: &BatchContext<'_>,
    storage: &mut Storage,
    batch: &[FileEvent],
    res: BatchResult,
) -> BatchStep {
    let BatchResult {
        commit_ok,
        failed,
        batch_len,
        extras_ok,
        extras_ms,
        started: batch_started,
        started_local: batch_started_local,
    } = res;

    // В disk-режиме (а worker сюда попадает всегда в disk после reopen на шаге 7)
    // flush_to_disk через Connection::backup() — бесполезное копирование БД самой
    // в себя, WAL не уменьшает. checkpoint_truncate реально схлопывает WAL.
    let wal_started = std::time::Instant::now();
    if let Err(e) = storage.checkpoint_truncate() {
        tracing::warn!("[{}] схлопывание журнала WAL не удалось: {}", ctx.path.display(), e);
    }
    // Схлопывание журнала WAL — то же самое, что «сброс на диск» у полной
    // индексации: отдельным этапом не выносим, показываем в итоговой строке.
    let flush_ms = wal_started.elapsed().as_millis();

    // Event-based cache invalidation (v0.9.1+): после успешного commit
    // отправляем cache-ci список затронутых относительных путей. Если
    // commit упал — invalidate не шлём (новых данных в индексе нет;
    // cache-ci пусть отдаёт что было, TTL подстрахует).
    if commit_ok {
        if let Some(cc) = ctx.cache_client {
            if !cc.is_empty() {
                let paths_to_invalidate = collect_invalidate_paths(ctx.path, batch);
                if !paths_to_invalidate.is_empty() {
                    let cc_clone = cc.clone();
                    let repo = ctx.entry.effective_alias();
                    tokio_block_on(async move {
                        cc_clone.invalidate_files(&repo, &paths_to_invalidate).await;
                    });
                }
            }
        }
    }

    // Батч отработан. Ready выставляем ТОЛЬКО если прошли ВСЕ три шага:
    // применение каждого события, фиксация транзакции и пересборка extras.
    // Провал любого из них означает неполный срез, а Ready на нём — ложное
    // «готово»: сервер выдачи начал бы отдавать устаревшее как актуальное.
    let outcome = batch_outcome(commit_ok, failed, batch_len, extras_ok);
    // Сводка того же вида, что у первичной индексации: начало, этапы, конец.
    // Печатается при любой настройке подробности.
    let stages = crate::logging::stages_take();
    let whole_ms = batch_started.elapsed().as_millis();
    let core_ms = stages
        .iter()
        .filter(|s| CORE_STAGES.contains(&s.name))
        .map(|s| s.dur.as_millis())
        .sum::<u128>();
    let mut parts = vec![format!("ядро {}", crate::logging::human_ms(core_ms))];
    if extras_ms > 0 {
        parts.push(format!("надстройка {}", crate::logging::human_ms(extras_ms)));
    }
    parts.push(format!("сброс на диск {}", crate::logging::human_ms(flush_ms)));

    tracing::info!(
        target: crate::logging::SUMMARY_TARGET,
        "[{}] частичная индексация, начало {}",
        ctx.path.display(),
        batch_started_local
    );
    for line in crate::logging::stages_block(&stages) {
        tracing::info!(target: crate::logging::SUMMARY_TARGET, "[{}] {}", ctx.path.display(), line);
    }
    match &outcome {
        Ok(()) => tracing::info!(
            target: crate::logging::SUMMARY_TARGET,
            "[{}] конец, обработано {} за {} ({}), режим хранилища на диске, память демона {}",
            ctx.path.display(),
            crate::logging::plural(batch_len as u64, "файл", "файла", "файлов"),
            crate::logging::human_ms(whole_ms),
            parts.join(", "),
            crate::logging::memory_note()
        ),
        Err(msg) => tracing::warn!(
            target: crate::logging::SUMMARY_TARGET,
            "[{}] конец, обработано НЕ ПОЛНОСТЬЮ: {} (сбоев {}), за {} ({}) — {}",
            ctx.path.display(),
            crate::logging::plural(batch_len as u64, "файл", "файла", "файлов"),
            failed,
            crate::logging::human_ms(whole_ms),
            parts.join(", "),
            msg
        ),
    }

    crate::logging::stage_idle();
    tokio_block_on(async {
        match outcome {
            Ok(()) => ctx.state.set_status(ctx.path, PathStatus::Ready).await,
            Err(msg) => ctx.state.set_error(ctx.path, msg).await,
        }
    });

    BatchStep::Continue
}

/// Выполнить initial reindex и запустить watcher-цикл для одной папки.
///
/// Функция блокирующая. Runner вызывает её через `spawn_blocking`. По завершении
/// (включая ошибку) статус папки уже записан в `DaemonState`.
///
/// `processor_registry` — список зарегистрированных `LanguageProcessor`-ов.
/// `None` означает «universal-only сборка» (`code-index.exe` без BSL); в этом
/// случае пропускаем `apply_schema_extensions` / `index_extras`. В сборке
/// `bsl-indexer.exe` сюда приходит registry с `BslLanguageProcessor`,
/// благодаря чему создаются специфичные таблицы (metadata_objects/...).
///
/// `cache_client` — клиент `mcp-cache-ci` для event-based invalidation
/// (этап 3, v0.9.1+). Если `None` или `is_empty()` — событийный канал не
/// используется, cache-ci работает только по TTL fallback. Если задан — после
/// каждого успешного `commit_batch()` worker асинхронно шлёт
/// `POST /invalidate {file_paths: [...]}` со списком файлов batch'а.
pub fn run_worker(
    entry: PathEntry,
    state: DaemonState,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
    initial_limiter: Option<Arc<Semaphore>>,
    indexer_section: IndexerSection,
    processor_registry: Option<Arc<ProcessorRegistry>>,
    cache_client: Option<Arc<CacheClient>>,
) {
    let path = match entry.path.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            tokio_block_on(async {
                state
                    .set_error(&entry.path, format!("Не удалось разрешить путь: {}", e))
                    .await;
            });
            return;
        }
    };

    // 1. Открыть/создать .code-index/index.db
    let db_dir = path.join(".code-index");
    if let Err(e) = std::fs::create_dir_all(&db_dir) {
        tokio_block_on(async {
            state
                .set_error(&path, format!("Создание .code-index/: {}", e))
                .await;
        });
        return;
    }
    let db_path = db_dir.join("index.db");

    // 2. Загрузить конфигурацию проекта (для exclude_dirs, debounce и т.п.)
    let mut index_config = match IndexConfig::load(&path) {
        Ok(c) => c,
        Err(e) => {
            tokio_block_on(async {
                state
                    .set_error(&path, format!("Загрузка IndexConfig: {}", e))
                    .await;
            });
            return;
        }
    };
    // Phase 2 (v0.8.0): эффективный лимит для file_contents.
    // Приоритет: per-path (`[[paths]].max_code_file_size_bytes`) →
    // глобальный `[indexer].max_code_file_size_bytes` → hardcoded 5 МБ.
    // Перетираем дефолт IndexConfig — переоформленные правила сильнее JSON-конфига проекта.
    index_config.max_code_file_size_bytes = entry.effective_max_code_file_size(&indexer_section);
    // Порог выбора способа обработки пачки — тем же порядком приоритета:
    // per-path → глобальный `[indexer]` → дефолт. Настройки демона сильнее
    // JSON-конфига проекта: правит их тот же человек, что и остальной daemon.toml.
    index_config.bulk_batch_threshold = entry.effective_bulk_batch_threshold(&indexer_section);
    // Язык репозитория из `[[paths]] language` — разрешает неоднозначные
    // расширения (`.h`: заголовок C или C++). Демон заполняет это поле на
    // старте автоопределением, если в конфиге его не указали.
    index_config.repo_language = entry.language.clone();
    let mut storage_config = StorageConfig {
        mode: index_config.storage_mode.clone(),
        memory_max_percent: index_config.memory_max_percent,
        // Заполним ниже, когда дойдём до открытия базы: оценка стоит пары
        // секунд обхода, и платить за неё, стоя в очереди, незачем.
        expected_bytes: 0,
    };

    // 3. Взять permit из семафора. Permit держится на всё время initial reindex,
    // включая открытие in-memory Storage — чтобы в памяти одновременно жил
    // максимум ОДИН in-memory storage (ограничено max_concurrent_initial).
    if let Some(sem) = initial_limiter.as_ref() {
        tracing::info!(
            "[{}] ожидание очереди на первичную индексацию (свободных мест {})",
            path.display(),
            sem.available_permits()
        );
    }
    let _permit = match tokio_block_on_value(acquire_initial_slot(initial_limiter)) {
        Ok(permit) => permit,
        Err(SlotClosed) => {
            // Семафор закрывают при остановке демона. Это штатное завершение:
            // выходим тихо. Паника здесь давала бы лишний перезапуск воркера
            // сторожем и мусорную запись в журнале, маскирующую настоящие аварии.
            tracing::info!(
                "[{}] очередь на первичную индексацию закрыта — демон останавливается, worker завершается",
                path.display()
            );
            return;
        }
    };

    // 4. Выставить статус InitialIndexing ПОСЛЕ получения permit — иначе
    // папки-кандидаты показываются как активно индексируются, хотя на самом
    // деле ждут своей очереди.
    tokio_block_on(async {
        state.set_status(&path, PathStatus::InitialIndexing).await;
        state.set_progress(&path, Progress::new(0, 0)).await;
        // Отметить свой поток: пока он занят долгой синхронной работой,
        // сам он состояние не обновит, и строка состояния демона берёт
        // текущий этап по этой отметке.
        state.note_worker_thread(&path).await;
    });

    // 5. Открыть Storage.
    //    * Если БД уже существует — сразу disk-режим. fast-path почти ничего
    //      не пишет, нет лишнего backup memory→disk (WAL не раздувается).
    //    * Если БД новая (первый запуск на этой папке) — in-memory для
    //      скорости, потом flush на диск и reopen в disk для watcher'а.
    // Проиндексирована ли папка — по наличию записей, а не по наличию файла.
    // Пустой файл базы заранее создаёт сервер выдачи, и от порядка запуска
    // служб зависит, успеет он это сделать до старта воркера или нет (на узле
    // сети сервер стартует ПОСЛЕ демона, на рабочей станции — раньше).
    // По этому признаку выбирается и режим хранилища, и слова в журнале —
    // иначе одна и та же папка ведёт себя на двух машинах по-разному.
    let db_has_rows = db_has_data(&db_path);

    // Паспорт папки: по присланному журналу должно быть видно, с чем работали
    // и при каких настройках, иначе времена не с чем соотнести.
    tracing::info!(
        "[{}] размер базы {}, настройки: порог перехода с пофайловой обработки на пакетную — \
         {} файлов, пауза после события {} мс, потолок сбора {} мс, транзакция записи {} файлов",
        path.display(),
        match std::fs::metadata(&db_path) {
            Ok(m) => format!("{} МБ", m.len() / (1024 * 1024)),
            Err(_) => "база ещё не создана".to_string(),
        },
        index_config.bulk_batch_threshold,
        entry.debounce_ms.unwrap_or(index_config.debounce_ms),
        entry.batch_ms.unwrap_or(index_config.batch_ms),
        index_config.batch_size
    );

    // Работала ли база в оперативной памяти. От этого зависит, нужен ли сброс
    // на диск после индексации, и что писать в итоге. Признак «файл базы есть»
    // для этого не годится: пустышки создаёт сервер выдачи, и успеет он это
    // сделать до старта воркера или нет — зависит от порядка запуска служб
    // (на узле сети сервер стартует ПОСЛЕ демона, на рабочей станции —
    // раньше). Режим должен быть один и тот же в обоих случаях.
    let mut worked_in_memory = false;

    let mut storage = if db_has_rows {
        tracing::info!(
            "[{}] база уже существует — режим хранилища: на диске (память демона {})",
            path.display(),
            crate::logging::memory_note()
        );
        match Storage::open_file(&db_path) {
            Ok(s) => s,
            Err(e) => {
                tokio_block_on(async {
                    state.set_error(&path, format!("Storage::open_file: {}", e)).await;
                });
                return;
            }
        }
    } else {
        // Фактический режим считаем той же функцией, что и `open_auto`, —
        // для новой базы (размер 0) он определён однозначно, расхождения с
        // тем, что реально откроется, быть не может.
        // Взвешиваем папку: только по её весу и можно судить, потянет ли
        // машина работу в памяти. Размер пустого файла базы об этом молчит.
        let weigh_started = std::time::Instant::now();
        let source_bytes = crate::indexer::estimate_source_bytes(&path, &index_config);
        storage_config.expected_bytes =
            source_bytes.saturating_mul(crate::indexer::MEMORY_ESTIMATE_FACTOR);

        let planned = crate::storage::memory::determine_storage_mode(&storage_config, &db_path);
        worked_in_memory = matches!(planned, crate::storage::memory::StorageMode::InMemory);
        tracing::info!(
            "[{}] новая база — режим хранилища: {} (настройка «{}»; исходники {}, \
             работа в памяти обошлась бы примерно в {}, свободно {}, разрешено занять {} % — \
             взвешивание заняло {} мс)",
            path.display(),
            storage_mode_ru(&planned),
            storage_config.mode,
            crate::logging::human_bytes(source_bytes),
            crate::logging::human_bytes(storage_config.expected_bytes),
            crate::logging::human_bytes(crate::storage::memory::available_ram()),
            storage_config.memory_max_percent,
            weigh_started.elapsed().as_millis()
        );
        match Storage::open_auto(&db_path, &storage_config) {
            Ok(s) => s,
            Err(e) => {
                tokio_block_on(async {
                    state.set_error(&path, format!("Storage::open_auto: {}", e)).await;
                });
                return;
            }
        }
    };

    // 5a. Применить schema_extensions процессора, соответствующего этому репо.
    //     Двухступенчатый resolve: явный `language` из daemon.toml → fallback
    //     на auto-detect по маркерам корня. DDL идемпотентен (`IF NOT EXISTS`),
    //     повторный вызов на каждом старте безопасен.
    //
    //     Без этого вызова в сборке `bsl-indexer.exe` BSL-tools падают с
    //     `no such table: metadata_objects` (см. v0.8.0 регрессия —
    //     apply_schema_extensions раньше вызывался только в CLI-команде Index).
    let resolved_processor = processor_registry
        .as_ref()
        .and_then(|reg| reg.resolve(entry.language.as_deref(), &path).cloned());
    if let Some(proc) = resolved_processor.as_ref() {
        // 5a-0. Догнать схему существующей БД (idempotent ALTER) ДО
        //       apply_schema_extensions: иначе CREATE INDEX по новой колонке
        //       рвёт DDL-батч на БД, созданной старым бинарником.
        if let Err(e) = proc.migrate_schema(storage.conn()) {
            tracing::warn!(
                "[{}] миграция схемы процессора «{}» упала: {}",
                path.display(), proc.name(), e
            );
        }
        let exts = proc.schema_extensions();
        if !exts.is_empty() {
            if let Err(e) = storage.apply_schema_extensions(exts) {
                tracing::warn!(
                    "[{}] расширение схемы процессора «{}» упало: {}. \
                     Базовая индексация продолжится, но инструменты надстройки могут не работать.",
                    path.display(), proc.name(), e
                );
            } else {
                tracing::info!(
                    "[{}] схема процессора «{}» применена ({} команд)",
                    path.display(), proc.name(), exts.len()
                );
            }
        }
    }

    // Называем вещи своими именами: на существующей базе это НЕ первичная
    // индексация, а сверка при старте — время и размер каждого файла
    // сравниваются с записанным, переиндексируются только разошедшиеся.
    crate::logging::block_separator();
    if db_has_rows {
        tracing::info!(
            "[{}] начата проверка изменений при старте: сверяю время и размер файлов с базой",
            path.display()
        );
    } else {
        tracing::info!(
            "[{}] начата первичная индексация: базы ещё нет, читаю все файлы",
            path.display()
        );
    }
    // Полное время работы с базой считаем отсюда: пользователю нужно знать,
    // сколько папка была занята целиком, а не сколько отработало одно ядро
    // индексатора. Накопитель этапов чистим, чтобы в раскладку не попали
    // этапы прошлого прохода этого же потока.
    let whole_started = std::time::Instant::now();
    let started_at_local = crate::logging::local_hms();
    crate::logging::stages_reset();

    // 6. Полная переиндексация (fast-path по mtime, если БД уже есть).
    //    Первичная индексация демона — полный путь (НЕ инкремент), поэтому
    //    сборщик extras BSL здесь уместен (инкрементальные обновления идут
    //    через index_extras_for_files и сборщик не задействуют).
    let parse_collector = resolved_processor
        .as_ref()
        .and_then(|proc| proc.parse_collector());
    let indexer_result = {
        let mut indexer = Indexer::with_config(&mut storage, index_config.clone());
        indexer.full_reindex_with_collector(&path, false, parse_collector.as_deref())
    };
    let core_stages = crate::logging::stages_take();
    let reindex = match indexer_result {
        Ok(result) => {
            tracing::info!(
                "[{}] {}: просмотрено {} файлов за {} мс — записано {}, без изменений {}, \
                 не индексируется {}, удалено {}",
                path.display(),
                if db_has_rows { "проверка изменений при старте закончена" } else { "первичная индексация закончена" },
                result.files_scanned,
                result.elapsed_ms,
                result.files_indexed,
                result.files_skipped,
                result.files_not_indexable,
                result.files_deleted
            );
            result
        }
        Err(e) => {
            tokio_block_on(async {
                state.set_error(&path, format!("full_reindex: {}", e)).await;
            });
            return;
        }
    };

    // 6a. index_extras процессора — для BSL это парсинг Configuration.xml /
    //     Forms / EventSubscriptions и заполнение metadata_*-таблиц.
    //
    //     ВАЖНО: вызывается ДО flush_to_disk. Если БД была новой и открыта
    //     in-memory — записи extras должны попасть в snapshot до сброса на
    //     диск, иначе исчезнут при reopen. Для disk-режима порядок не важен,
    //     но единый код проще.
    //
    //     Ошибка не фатальна: базовая индексация уже сохранена. Логируем и
    //     продолжаем — например, для репо без Configuration.xml (старая
    //     выгрузка обработок) парсер может ничего не найти и это нормально.
    // Сколько заняла надстройка целиком — для раскладки в итоге. Ноль
    // означает, что её не пересобирали (данные не менялись).
    let mut extras_ms: u128 = 0;
    if let Some(proc) = resolved_processor.as_ref() {
        // Гейт против холостого re-enrichment на старте: если БД уже была и
        // mtime-fast-path не нашёл изменений (0 записано / 0 удалено), а extras
        // процессора уже наполнены — полный index_extras пропускаем (он дорогой:
        // перестроение metadata_*/terms/графа на больших конфигурациях занимает
        // минуты). Любое изменение данных, новая БД или пустые extras → полный
        // проход как раньше. Инкрементальные правки покрывает watcher-цикл через
        // index_extras_for_files.
        let skip_extras = db_has_rows
            && reindex.files_indexed == 0
            && reindex.files_deleted == 0
            && proc.extras_present(&storage);

        // Между «ничего не менялось» и «полный пересбор» есть третий случай:
        // изменилась горстка файлов, и их пути известны. Тогда зовём тот же
        // точечный путь, которым идёт watcher-цикл, — полный пересбор на
        // больших конфигурациях стоит минуты недоступности (замер: 6 файлов из
        // 94 650 → 15,7 минуты), точечный отрабатывает за секунды. Полный
        // остаётся для новой БД, пустой надстройки и переполнения списка путей.
        let incremental_extras = !skip_extras
            && db_has_rows
            && !reindex.paths_overflow
            && proc.extras_present(&storage);

        let mut need_full = !skip_extras;
        if skip_extras {
            tracing::info!(
                "[{}] надстройка не пересобирается: данные не менялись (быстрая проверка по времени файлов), \
                 надстройка процессора «{}» уже на месте",
                path.display(), proc.name()
            );
        } else if incremental_extras {
            let changed: Vec<PathBuf> =
                reindex.changed_paths.iter().map(|p| path.join(p)).collect();
            let deleted: Vec<PathBuf> =
                reindex.deleted_paths.iter().map(|p| path.join(p)).collect();
            let t0 = std::time::Instant::now();
            let incremental_outcome =
                proc.index_extras_for_files(&path, &mut storage, &changed, &deleted);
            extras_ms = t0.elapsed().as_millis();
            match incremental_outcome {
                Ok(()) => {
                    need_full = false;
                    tracing::info!(
                        "[{}] надстройка процессора «{}» обновлена точечно на старте за {} мс \
                         (изменено {}, удалено {})",
                        path.display(), proc.name(), t0.elapsed().as_millis(),
                        changed.len(), deleted.len()
                    );
                }
                Err(e) => {
                    // Точечный путь оставляет надстройку в неизвестном состоянии,
                    // поэтому падение лечится полным пересбором, а не пропуском.
                    tracing::warn!(
                        "[{}] точечное обновление надстройки процессора «{}» на старте упало: {}. \
                         Переходим на полный пересбор.",
                        path.display(), proc.name(), e
                    );
                }
            }
        }

        if need_full {
            let t0 = std::time::Instant::now();
            // Сообщаем о НАЧАЛЕ: на больших конфигурациях полный пересбор идёт
            // минутами, и если он встанет — в журнале должна остаться запись
            // о том, на чём именно встали, а не тишина после прошлой строки.
            tracing::info!(
                "[{}] начат полный пересбор надстройки процессора «{}» \
                 (на больших конфигурациях занимает минуты)",
                path.display(), proc.name()
            );
            let full_outcome = proc.index_extras(&path, &mut storage);
            extras_ms = t0.elapsed().as_millis();
            if let Err(e) = full_outcome {
                tracing::warn!(
                    "[{}] полный пересбор надстройки процессора «{}» упал: {}. \
                     Базовая индексация при этом сохранена.",
                    path.display(), proc.name(), e
                );
            } else {
                tracing::info!(
                    "[{}] полный пересбор надстройки процессора «{}» выполнен за {} мс",
                    path.display(), proc.name(), extras_ms
                );
            }
        }
    }

    // 7. Если БД была новой и открылась в памяти — flush + reopen в disk.
    //    Если уже был disk — ничего делать не нужно, изменения уже на диске.
    let extras_stages = crate::logging::stages_take();
    let flush_started = std::time::Instant::now();
    if worked_in_memory {
        if let Err(e) = storage.flush_to_disk(&db_path) {
            tracing::warn!("[{}] сброс базы из памяти на диск не удался: {}", path.display(), e);
        }
        drop(storage);
        storage = match Storage::open_file(&db_path) {
            Ok(s) => s,
            Err(e) => {
                tokio_block_on(async {
                    state.set_error(&path, format!("Storage::open_file (disk reopen): {}", e)).await;
                });
                return;
            }
        };
        tracing::info!("[{}] база переоткрыта на диске", path.display());

        // База в памяти закрыта — просим распределитель вернуть её системе.
        // Без этого освобождённое остаётся в куче процесса, следующая папка
        // видит память как занятую и уходит на диск, хотя машина потянула бы.
        // Печатаем «было/стало»: решение о режиме принимается по свободной
        // памяти, и по журналу должно быть видно, вернулась она или нет.
        let before = crate::logging::process_memory_bytes();
        let released = crate::storage::memory::release_free_memory();
        let after = crate::logging::process_memory_bytes();
        match (before, after) {
            (Some(b), Some(a)) => tracing::info!(
                "[{}] память после работы в памяти: было {}, стало {} (возврат системе: {})",
                path.display(),
                crate::logging::human_bytes(b),
                crate::logging::human_bytes(a),
                if released { "выполнен" } else { "система такого не умеет" }
            ),
            _ => tracing::info!(
                "[{}] возврат памяти системе: {}",
                path.display(),
                if released { "выполнен" } else { "система такого не умеет" }
            ),
        }
    }

    // Initial reindex мог накопить много страниц в WAL (особенно для больших
    // репо с 90k+ файлов). `PRAGMA wal_autocheckpoint=500` не гарантирует
    // физическое уменьшение файла — нужен явный TRUNCATE.
    match storage.checkpoint_truncate() {
        Ok((busy, log_pages, _)) if busy == 0 => {
            tracing::info!(
                "[{}] журнал WAL схлопнут: вытеснено страниц {}",
                path.display(), log_pages
            );
        }
        Ok((busy, _, _)) => {
            tracing::info!(
                "[{}] журнал WAL схлопнут частично (занято читателями: {})",
                path.display(), busy
            );
        }
        Err(e) => {
            tracing::warn!(
                "[{}] схлопывание журнала WAL после первичной индексации не удалось: {}",
                path.display(), e
            );
        }
    }

    // Итог по папке — то, что первым делом ищут в присланном журнале.
    // Первым числом идёт ПОЛНОЕ время работы с базой: ядро индексатора,
    // надстройка процессора и сброс на диск вместе. Прежде здесь стояло время
    // одного ядра, и на больших конфигурациях оно занижало правду в разы —
    // надстройка растёт быстрее ядра.
    let flush_ms = flush_started.elapsed().as_millis();
    let whole_ms = whole_started.elapsed().as_millis();
    let others_ms = whole_ms.saturating_sub(
        reindex.elapsed_ms as u128 + extras_ms + flush_ms,
    );
    let mut parts = vec![
        format!("ядро {}", crate::logging::human_ms(reindex.elapsed_ms as u128)),
    ];
    if extras_ms > 0 {
        parts.push(format!("надстройка {}", crate::logging::human_ms(extras_ms)));
    }
    parts.push(format!("сброс на диск {}", crate::logging::human_ms(flush_ms)));
    // «Прочее» — освобождение памяти под разобранные файлы и переходы между
    // шагами. Показываем, только когда оно заметно, иначе слагаемые не сходятся
    // с полным временем и в это упирается первый же читатель журнала.
    if others_ms >= 1000 {
        parts.push(format!("прочее {}", crate::logging::human_ms(others_ms)));
    }

    // Короткая сводка: что за папка, когда начали, что делали по этапам, когда
    // закончили. Печатается при любой настройке подробности — метка «итог»
    // пропускается фильтром всегда.
    let mut stages = core_stages;
    stages.extend(extras_stages);
    tracing::info!(
        target: crate::logging::SUMMARY_TARGET,
        "[{}] {}, начало {}",
        path.display(),
        if db_has_rows { "проверка изменений при старте" } else { "первичная индексация" },
        started_at_local
    );
    for line in crate::logging::stages_block(&stages) {
        tracing::info!(target: crate::logging::SUMMARY_TARGET, "[{}] {}", path.display(), line);
    }
    tracing::info!(
        target: crate::logging::SUMMARY_TARGET,
        "[{}] конец, обработано {} файлов за {} ({}), режим хранилища {}, память демона {}",
        path.display(),
        reindex.files_scanned,
        crate::logging::human_ms(whole_ms),
        parts.join(", "),
        if worked_in_memory {
            "в оперативной памяти со сбросом на диск"
        } else {
            "на диске"
        },
        crate::logging::memory_note()
    );

    // 9. Отпустить permit — следующий worker может начинать initial reindex.
    drop(_permit);

    // 10. Перевести в Ready и запустить watcher
    crate::logging::stage_idle();
    tokio_block_on(async {
        state.set_status(&path, PathStatus::Ready).await;
    });

    // 8. Watcher-цикл
    let debounce_ms = entry.debounce_ms.unwrap_or(index_config.debounce_ms);
    let batch_ms = entry.batch_ms.unwrap_or(index_config.batch_ms);
    let watcher_config = WatcherConfig {
        debounce_ms,
        batch_ms,
        exclude_dirs: index_config.exclude_dirs.clone(),
        exclude_file_patterns: index_config.exclude_file_patterns.clone(),
        repo_language: index_config.repo_language.clone(),
        extra_text_extensions: index_config.extra_text_extensions.clone(),
    };
    let (watcher, rx) = match create_watcher(&path, &watcher_config) {
        Ok(pair) => pair,
        Err(e) => {
            tokio_block_on(async {
                state.set_error(&path, format!("create_watcher: {}", e)).await;
            });
            return;
        }
    };
    // Держим watcher на стеке — при drop watcher остановится.
    let _watcher = watcher;

    tracing::info!(
        "[{}] слежение за файлами включено (пауза после события {} мс, окно пакета {} мс)",
        path.display(), debounce_ms, batch_ms
    );

    let registry = ParserRegistry::from_languages(&index_config.languages);
    // Эффективный лимит для file_contents — пробросим в apply_event,
    // чтобы Indexer::with_config не пересоздавался на каждое событие.
    let max_code_file_size = index_config.max_code_file_size_bytes;
    let repo_language = index_config.repo_language.clone();
    let extra_text_extensions = index_config.extra_text_extensions.clone();

    // Основной цикл обработки батчей. Idle-таймаут 500 мс даёт шанс проверить
    // shutdown-сигнал даже если файлов давно не меняли.
    const IDLE_POLL_MS: u64 = 500;
    // Обвязка для обработки пакетов — собирается один раз на весь цикл.
    let ctx = BatchContext {
        path: &path,
        entry: &entry,
        state: &state,
        registry: &registry,
        max_code_file_size,
        repo_language: repo_language.as_deref(),
        extra_text_extensions: &extra_text_extensions,
        resolved_processor: resolved_processor.as_ref(),
        cache_client: cache_client.as_ref(),
        index_config: &index_config,
    };

    loop {
        if shutdown_received(&mut shutdown_rx) {
            break;
        }

        let collected = match poll_batch(&rx, IDLE_POLL_MS, debounce_ms, batch_ms) {
            Ok(Some(b)) => {
                // Число изменившихся файлов точное только когда сбор закончился
                // тишиной. Упёрлись в потолок — поток ещё идёт, и в очереди
                // осталось; говорим об этом прямо, иначе число в журнале
                // прочтут как итог.
                // Черта — здесь: сбор изменений уже начало новой работы, и
                // отделять надо от предыдущей операции, а не от собственной
                // первой строки.
                crate::logging::block_separator();
                if b.settled {
                    tracing::info!(
                        "[{}] наблюдатель собрал изменения: {} (поток событий утих, \
                         это все изменения)",
                        path.display(),
                        crate::logging::plural(b.events.len() as u64, "файл", "файла", "файлов")
                    );
                } else {
                    tracing::info!(
                        "[{}] наблюдатель собрал изменения: {} — поток событий не утих за {} мс, \
                         остальные попадут в следующую пачку",
                        path.display(),
                        crate::logging::plural(b.events.len() as u64, "файл", "файла", "файлов"),
                        batch_ms
                    );
                }
                b
            }
            Ok(None) => continue, // idle timeout — проверим shutdown на следующей итерации
            Err(_) => break,      // канал закрыт — watcher дропнут
        };
        let batch = collected.events;
        if batch.is_empty() {
            continue;
        }

        match process_batch(&ctx, &mut storage, &batch) {
            BatchStep::Continue => {}
            BatchStep::Stop => break,
        }
    }

    tracing::info!("[{}] остановка worker'а, завершающее схлопывание журнала WAL", path.display());
    if let Err(e) = storage.checkpoint_truncate() {
        tracing::warn!("[{}] завершающее схлопывание журнала WAL не удалось: {}", path.display(), e);
    }
}

/// Семафор слотов initial reindex закрыт — демон останавливается.
#[derive(Debug)]
struct SlotClosed;

/// Взять слот initial reindex. Вынесено отдельной функцией ради модульного
/// теста — тем же приёмом, что `batch_outcome` и `allow_respawn`.
///
/// * `Ok(None)` — ограничение не задано, слот не нужен;
/// * `Ok(Some(permit))` — слот получен, держится на всё время initial reindex;
/// * `Err(SlotClosed)` — семафор закрыт, воркеру надо тихо завершиться.
///
/// Закрытие семафора бывает только при остановке демона, поэтому это штатный
/// исход, а не нарушенный инвариант: паника на нём давала лишний перезапуск
/// воркера сторожем и запись в журнале, маскирующую настоящие аварии.
async fn acquire_initial_slot(
    limiter: Option<Arc<Semaphore>>,
) -> Result<Option<OwnedSemaphorePermit>, SlotClosed> {
    match limiter {
        None => Ok(None),
        Some(sem) => sem.acquire_owned().await.map(Some).map_err(|_| SlotClosed),
    }
}

/// Решение о статусе папки по итогам батча. Вынесено чистой функцией ради
/// модульного теста: «готово» допустимо ТОЛЬКО когда прошли все три шага —
/// применение каждого события, фиксация транзакции и пересборка extras.
///
/// Провал любого из них означает неполный или рассогласованный срез. Ready на
/// нём был бы ложным «готово»: сервер выдачи начал бы отдавать устаревшее как
/// актуальное, и признака этого в ответе нет.
///
/// `Ok(())` — можно выставлять Ready. `Err(текст)` — причина, по которой папка
/// помечается сбойной; текст уходит в поле `error` состояния демона.
fn batch_outcome(
    commit_ok: bool,
    failed: usize,
    batch_len: usize,
    extras_ok: bool,
) -> Result<(), String> {
    if !commit_ok {
        return Err(
            "фиксация батча не удалась — данные не применены, см. журнал демона".to_string(),
        );
    }
    if failed > 0 {
        return Err(format!(
            "батч применён частично: {} из {} событий не удалось — эти файлы \
             остались в индексе прежней версии, см. журнал демона",
            failed, batch_len
        ));
    }
    if !extras_ok {
        return Err("базовый индекс обновлён, но слой extras не пересобран — граф вызовов \
             и связи данных отстают, см. журнал демона"
            .to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn готово_только_когда_прошли_все_три_шага() {
        assert!(batch_outcome(true, 0, 5, true).is_ok());
    }

    /// Ограничение не задано — слот не нужен, воркер идёт дальше без ожидания.
    #[tokio::test]
    async fn без_ограничения_слот_не_требуется() {
        assert!(acquire_initial_slot(None).await.unwrap().is_none());
    }

    /// Обычный ход: свободный слот выдаётся.
    #[tokio::test]
    async fn свободный_слот_выдаётся() {
        let sem = Arc::new(Semaphore::new(1));
        assert!(acquire_initial_slot(Some(sem)).await.unwrap().is_some());
    }

    /// Регресс S-4: на закрытом семафоре раньше была паника `expect("semaphore
    /// closed")`. Закрывают его только при остановке демона — это штатный
    /// исход, воркер обязан выйти тихо.
    #[tokio::test]
    async fn закрытый_семафор_даёт_ошибку_а_не_панику() {
        let sem = Arc::new(Semaphore::new(1));
        sem.close();
        assert!(matches!(
            acquire_initial_slot(Some(sem)).await,
            Err(SlotClosed)
        ));
    }

    #[test]
    fn провал_фиксации_не_даёт_готово() {
        let err = batch_outcome(false, 0, 5, true).unwrap_err();
        assert!(err.contains("фиксация"), "текст: {err}");
    }

    /// Регресс S-2: одна неудачная запись раньше молча тонула в журнале, а папка
    /// объявлялась готовой на неполном срезе.
    #[test]
    fn частично_применённый_батч_не_даёт_готово() {
        let err = batch_outcome(true, 2, 7, true).unwrap_err();
        assert!(err.contains("2 из 7"), "текст: {err}");
    }

    /// Регресс S-3: тела функций свежие, граф вызовов отстал — «готово» на таком
    /// срезе вводит в заблуждение, и признака рассогласования в выдаче нет.
    #[test]
    fn провал_пересборки_extras_не_даёт_готово() {
        let err = batch_outcome(true, 0, 3, false).unwrap_err();
        assert!(err.contains("extras"), "текст: {err}");
    }

    /// Когда провалено несколько шагов сразу — сообщаем о самом раннем:
    /// без фиксации остальные причины вторичны.
    #[test]
    fn провал_фиксации_важнее_прочих_причин() {
        let err = batch_outcome(false, 3, 3, false).unwrap_err();
        assert!(err.contains("фиксация"), "текст: {err}");
    }

    /// Пустая база, созданная сервером выдачи заранее, не должна считаться
    /// уже проиндексированной: иначе первичная индексация выдаётся в журнале
    /// за сверку изменений, а хранилище открывается на диске вместо памяти.
    #[test]
    fn пустая_база_от_сервера_выдачи_данными_не_считается() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("index.db");

        assert!(!db_has_data(&db), "файла ещё нет — данных нет");

        // Ровно то, что делает сервер выдачи: создать файл со схемой и закрыть.
        drop(Storage::open_file(&db).unwrap());
        assert!(db.exists(), "файл базы создан");
        assert!(!db_has_data(&db), "схема без записей — это не проиндексированная папка");

        let storage = Storage::open_file(&db).unwrap();
        storage
            .conn()
            .execute(
                "INSERT INTO files (path, content_hash, language) VALUES ('a.rs', 'h', 'rust')",
                [],
            )
            .unwrap();
        drop(storage);
        assert!(db_has_data(&db), "появилась запись о файле — база рабочая");
    }
}

fn shutdown_received(rx: &mut tokio::sync::broadcast::Receiver<()>) -> bool {
    matches!(rx.try_recv(), Ok(()))
}

/// Собрать список относительных file_path из batch'а FS-событий для отправки
/// в `cache-ci` через `POST /invalidate {file_paths}`.
///
/// Используются ВСЕ типы событий — Modified/Created/Deleted: cache_entries,
/// зависящие от удалённого файла, также должны быть снесены. Дубликаты
/// (несколько событий по одному файлу в одном batch) дедуплицируются.
/// Пути приводятся к forward-slash формату (совпадает с тем, что daemon
/// записал в SQLite через `rel_path.replace('\\', "/")`).
fn collect_invalidate_paths(root: &PathBuf, batch: &[FileEvent]) -> Vec<String> {
    use std::collections::HashSet;
    let mut set: HashSet<String> = HashSet::new();
    for event in batch {
        let abs = match event {
            FileEvent::Modified(p) | FileEvent::Created(p) | FileEvent::Deleted(p) => p,
        };
        let rel = abs
            .strip_prefix(root)
            .unwrap_or(abs)
            .to_string_lossy()
            .replace('\\', "/");
        if !rel.is_empty() {
            set.insert(rel);
        }
    }
    set.into_iter().collect()
}

/// Собрать (rel_path, observed_mtime) для Modified/Created событий батча — вход
/// для раннего `mark-dirty` (write-triggered ленивая ревалидация, #1471).
///
/// Deleted пропускаем: mtime у удалённого файла нет, его кэш-записи закрывает
/// `invalidate` после commit. `mtime` читается прямым `stat` (worker co-located
/// с файлами) — unix-секунды, та же семантика, что у `files.mtime` в индексе.
/// Дедуп по пути; при нескольких событиях по одному файлу берём максимум mtime.
/// Forward-slash формат совпадает с rel_path в SQLite и с `dependent_files`.
fn collect_dirty_paths(root: &PathBuf, batch: &[FileEvent]) -> Vec<(String, i64)> {
    use std::collections::HashMap;
    let mut map: HashMap<String, i64> = HashMap::new();
    for event in batch {
        let abs = match event {
            FileEvent::Modified(p) | FileEvent::Created(p) => p,
            FileEvent::Deleted(_) => continue,
        };
        let mtime = match std::fs::metadata(abs)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
        {
            Some(m) => m,
            None => continue, // файл уже исчез (atomic save .tmp→rename) — пропустим
        };
        let rel = abs
            .strip_prefix(root)
            .unwrap_or(abs)
            .to_string_lossy()
            .replace('\\', "/");
        if rel.is_empty() {
            continue;
        }
        map.entry(rel)
            .and_modify(|e| {
                if mtime > *e {
                    *e = mtime;
                }
            })
            .or_insert(mtime);
    }
    map.into_iter().collect()
}

fn tokio_block_on<F: std::future::Future<Output = ()>>(fut: F) {
    tokio_block_on_value::<(), F>(fut);
}

fn tokio_block_on_value<T, F: std::future::Future<Output = T>>(fut: F) -> T {
    // Worker запускается внутри spawn_blocking, поэтому tokio runtime существует
    // и мы можем получить текущий handle.
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.block_on(fut)
    } else {
        // На случай запуска вне tokio (тесты) — собираем одноразовый runtime.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("create fallback tokio runtime");
        rt.block_on(fut)
    }
}

/// Имена этапов ядра индексатора. Нужны, чтобы в итоге по пакету изменений
/// отделить время ядра от времени надстройки: этапы копятся вперемешку, а
/// разложить их надо так же, как при полной индексации.
const CORE_STAGES: [&str; 7] = [
    "разбор файлов",
    "запись в базу",
    "удаление исчезнувших",
    "сжатие содержимого",
    // Эти три даёт полный проход — им обрабатывается большая пачка изменений.
    "обход дерева и отбор файлов",
    "индексы и полнотекстовый поиск",
    "дозаполнение содержимого",
];

/// Обработать одно событие файловой системы: пересчитать хеш, записать/удалить в БД.
///
/// Возвращает `false`, если применить событие не удалось: файл не прочитан по
/// причине кроме NotFound, не разобран парсером, не записан в БД или не удалён
/// из неё. Вызывающий считает такие случаи и не выдаёт по батчу статус «готово»:
/// индекс в этом месте остался на прежнем срезе, и «готово» было бы ложным.
///
/// Исчезнувший файл (NotFound) ошибкой НЕ считается — это штатный ход
/// atomic-save через `.tmp` → rename, иначе каждое сохранение из редактора
/// помечало бы папку сбойной.
fn apply_event(
    storage: &mut Storage,
    root: &PathBuf,
    event: &FileEvent,
    registry: &ParserRegistry,
    max_code_file_size: usize,
    repo_language: Option<&str>,
    extra_text_extensions: &[String],
) -> bool {
    match event {
        FileEvent::Modified(abs) | FileEvent::Created(abs) => {
            // Событие на существующем каталоге = папку создали или переименовали.
            // Файлы внутри своих событий не получили — раскрываем обходом, иначе
            // содержимое новой папки не попадёт в индекс до перезапуска демона.
            if abs.is_dir() {
                return apply_dir_scan(
                    storage,
                    root,
                    abs,
                    registry,
                    max_code_file_size,
                    repo_language,
                    extra_text_extensions,
                );
            }
            let (content, hash, is_binary) = match hasher::file_hash(abs) {
                Ok(triple) => triple,
                Err(e) => {
                    // Частый случай: atomic-save через .tmp → rename. Watcher увидел
                    // событие на .tmp, но к моменту хэширования файл уже переименован.
                    // NotFound — не ошибка, тихо игнорируем.
                    if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                        if io_err.kind() == std::io::ErrorKind::NotFound {
                            return true;
                        }
                    }
                    // Пофайловые сбои — на уровне отладки: на сломанной выгрузке
                    // их тысячи, и в обычном журнале они топят всё остальное.
                    // Сводку по пакету даёт `batch_outcome`.
                    tracing::debug!("[{}] не прочитан {}: {}", root.display(), abs.display(), e);
                    return false;
                }
            };

            let meta = std::fs::metadata(abs).ok();
            let mtime = meta.as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            let file_size = meta.as_ref().map(|m| m.len() as i64);

            let rel_path = abs
                .strip_prefix(root)
                .unwrap_or(abs)
                .to_string_lossy()
                .replace('\\', "/");

            // Двоичный контент (EDT-защищённый модуль — .bsl с двоичным образом)
            // трактуем как Binary: не парсим, как и обычные бинарные файлы.
            let category = if is_binary {
                FileCategory::Binary
            } else {
                categorize_file_in_repo(abs, repo_language, extra_text_extensions)
            };
            // Исход применения события. Ошибка записи не должна молча
            // превращаться в «готово» на уровне батча — см. run_worker.
            let mut ok = true;
            match category {
                FileCategory::Code(language) => {
                    let ext = abs
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    if let Some(parser) = registry.get_parser(&ext) {
                        let parse_started = std::time::Instant::now();
                        let parsed = parser.parse_guarded(&content, &rel_path);
                        crate::logging::stage_add("разбор файлов", parse_started.elapsed());
                        let write_started = std::time::Instant::now();
                        match parsed {
                            Ok(pr) => {
                                let indexer = Indexer::with_config(
                                    storage,
                                    IndexConfig {
                                        max_code_file_size_bytes: max_code_file_size,
                                        ..IndexConfig::default()
                                    },
                                );
                                // v0.7.1: для html (и других dual-indexed языков) дополнительно пишем
                                // raw-content в text_files — чтобы search_text/grep_text/read_file
                                // продолжали работать как для обычного text-файла.
                                let text_for_fts = if crate::indexer::file_types::is_dual_indexed_language(&language) {
                                    Some(content.as_str())
                                } else {
                                    None
                                };
                                if let Err(e) = indexer.write_code_to_db(
                                    &rel_path,
                                    &hash,
                                    &language,
                                    pr.lines_total,
                                    &pr,
                                    false,
                                    mtime,
                                    file_size,
                                    text_for_fts,
                                    crate::indexer::ContentInput::Raw(content.as_str()),
                                ) {
                                    tracing::debug!("[{}] запись кода {}: {}",
                                        root.display(), rel_path, e);
                                    ok = false;
                                }
                            }
                            Err(e) => {
                                tracing::debug!("[{}] разбор {}: {}",
                                    root.display(), rel_path, e);
                                ok = false;
                            }
                        }
                        crate::logging::stage_add("запись в базу", write_started.elapsed());
                    }
                }
                FileCategory::Text => {
                    let text_started = std::time::Instant::now();
                    // Попробуем XML 1С — если есть BSL-блоки, пишем как код.
                    let ext = abs
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("");
                    let indexed_as_code = if ext == "xml" {
                        let xml_parser = crate::parser::xml_1c::Xml1CParser;
                        if let Ok(pr) = xml_parser.parse(&content, &rel_path) {
                            if !pr.functions.is_empty()
                                || !pr.classes.is_empty()
                                || !pr.variables.is_empty()
                            {
                                let indexer = Indexer::with_config(
                                    storage,
                                    IndexConfig {
                                        max_code_file_size_bytes: max_code_file_size,
                                        ..IndexConfig::default()
                                    },
                                );
                                indexer
                                    .write_code_to_db(
                                        &rel_path,
                                        &hash,
                                        "xml_1c",
                                        pr.lines_total,
                                        &pr,
                                        false,
                                        mtime,
                                        file_size,
                                        None,
                                        crate::indexer::ContentInput::Raw(content.as_str()),
                                    )
                                    .is_ok()
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if !indexed_as_code {
                        let tr = TextParser::parse(&content);
                        let indexer = Indexer::new(storage);
                        if let Err(e) = indexer.write_text_to_db(
                            &rel_path,
                            &hash,
                            tr.lines_total,
                            &tr.content,
                            false,
                            mtime,
                            file_size,
                        ) {
                            tracing::debug!("[{}] запись текста {}: {}",
                                root.display(), rel_path, e);
                            ok = false;
                        }
                    }
                    crate::logging::stage_add("запись в базу", text_started.elapsed());
                }
                FileCategory::Binary => {}
            }
            ok
        }
        FileEvent::Deleted(abs) => {
            let delete_started = std::time::Instant::now();
            let rel_path = abs
                .strip_prefix(root)
                .unwrap_or(abs)
                .to_string_lossy()
                .replace('\\', "/");
            let mut ok = true;
            if let Ok(Some(file)) = storage.get_file_by_path(&rel_path) {
                if let Some(id) = file.id {
                    // Провал удаления оставляет файл фантомом в выдаче —
                    // это расхождение индекса с диском, а не мелочь для тишины.
                    if let Err(e) = storage.delete_file(id) {
                        tracing::debug!("[{}] удаление из индекса {}: {}",
                            root.display(), rel_path, e);
                        ok = false;
                    }
                }
            } else {
                // Точной записи нет — значит исчез КАТАЛОГ: при переименовании
                // папки ОС шлёт одно событие на неё саму, файлы внутри своих
                // событий не получают. Без этого прохода они остались бы
                // фантомами в выдаче до полной переиндексации.
                match storage.delete_files_under_prefix(&rel_path) {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(
                        "[{}] каталог {} исчез — удалено файлов из индекса: {}",
                        root.display(), rel_path, n
                    ),
                    Err(e) => {
                        tracing::warn!("[{}] удаление файлов исчезнувшего каталога {}: {}",
                            root.display(), rel_path, e);
                        ok = false;
                    }
                }
            }
            crate::logging::stage_add("удаление исчезнувших", delete_started.elapsed());
            ok
        }
    }
}

/// Раскрыть каталог: применить «файл создан» к каждому файлу внутри, рекурсивно.
///
/// Вызывается, когда событие пришло на существующий каталог — так ОС сообщает о
/// созданной или переименованной папке, не присылая событий на файлы внутри.
///
/// Исключённые каталоги (`.git`, `.code-index` и прочие из `EXCLUDE_DIRS`)
/// пропускаем: без этого демон поймал бы собственные записи WAL и ушёл в петлю
/// переиндексации.
///
/// Возвращает `false`, если хотя бы один файл внутри применить не удалось.
fn apply_dir_scan(
    storage: &mut Storage,
    root: &PathBuf,
    dir: &Path,
    registry: &ParserRegistry,
    max_code_file_size: usize,
    repo_language: Option<&str>,
    extra_text_extensions: &[String],
) -> bool {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            // Каталог мог исчезнуть между событием и обходом — это гонка, а не
            // поломка индекса: сообщение о его удалении придёт своим событием.
            if e.kind() == std::io::ErrorKind::NotFound {
                return true;
            }
            tracing::debug!("[{}] обход каталога {}: {}", root.display(), dir.display(), e);
            return false;
        }
    };

    let mut ok = true;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let excluded = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| crate::indexer::file_types::EXCLUDE_DIRS.contains(&n))
                .unwrap_or(false);
            if excluded {
                continue;
            }
            if !apply_dir_scan(
                storage,
                root,
                &path,
                registry,
                max_code_file_size,
                repo_language,
                extra_text_extensions,
            ) {
                ok = false;
            }
        } else if path.is_file()
            && !apply_event(
                storage,
                root,
                &FileEvent::Created(path.clone()),
                registry,
                max_code_file_size,
                repo_language,
                extra_text_extensions,
            )
        {
            ok = false;
        }
    }
    ok
}
