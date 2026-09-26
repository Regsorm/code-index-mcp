// Пул read-only SQLite-соединений на один репозиторий.
//
// Зачем: до пула каждый репо обслуживался ОДНИМ `Connection` под
// `tokio::sync::Mutex<Storage>`. Любой tool брал этот мьютекс на всё время
// обработки, поэтому тяжёлый запрос (`bsl_sql` до 8с, полный `grep_code`,
// рекурсивные обходы графа) задерживал все остальные запросы к ТОМУ ЖЕ репо,
// даже мгновенный `get_function`. Пул держит несколько read-only соединений к
// одному `index.db`, и несколько чтений идут одновременно (SQLite в режиме WAL
// рассчитан на много читателей).
//
// Соединения только на чтение, открываются лениво до `max_size`. Семафор
// ограничивает число одновременно выданных соединений. Возврат соединения в
// пул — по Drop guard'а (`PooledStorage`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::Storage;

/// Дефолты пула (если не заданы в `serve.toml [pool]`).
pub const DEFAULT_POOL_SIZE: usize = 4;
/// 16 МБ page-cache на соединение. 4 × 16 = 64 МБ на активный репо — столько же,
/// сколько у нынешнего единственного соединения (`cache_size=-64000`).
pub const DEFAULT_PER_CONN_CACHE_KIB: usize = 16_384;
/// busy_timeout: краткая блокировка при checkpoint/backup демоном переждётся,
/// а не превратится в `SQLITE_BUSY` (по умолчанию busy_timeout=0 — без ожидания).
pub const DEFAULT_BUSY_TIMEOUT_MS: u32 = 5_000;

tokio::task_local! {
    /// Метка текущей выдачи соединения — «инструмент|репо».
    ///
    /// Ставит обработчик вызова инструмента (`CodeIndexServer::call_tool`), чтобы
    /// предупреждение о затянувшейся выдаче называло виновника по имени, а не
    /// оставалось безымянным. Вне области видимости (тесты, служебные пути) метки
    /// нет — учёт покажет «без метки».
    pub static CHECKOUT_LABEL: String;
}

/// Порог, после которого выдача соединения считается затянувшейся, — 60 секунд.
///
/// Самые долгие законные запросы ограничены 8 секундами (`bsl_sql`,
/// `find_references`, `get_data_links`, `get_object_profile`) — порог взят с
/// большим запасом.
pub const LONG_CHECKOUT_WARN: Duration = Duration::from_secs(60);

/// Параметры пула на репозиторий.
#[derive(Debug, Clone, Copy)]
pub struct PoolConfig {
    /// Максимум одновременно открытых соединений (= число параллельных чтений).
    pub max_size: usize,
    /// Размер page-cache на одно соединение, КиБ (переопределяет дефолтный -64000).
    pub cache_kib: usize,
    /// busy_timeout соединения, мс.
    pub busy_timeout_ms: u32,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_size: DEFAULT_POOL_SIZE,
            cache_kib: DEFAULT_PER_CONN_CACHE_KIB,
            busy_timeout_ms: DEFAULT_BUSY_TIMEOUT_MS,
        }
    }
}

impl PoolConfig {
    /// Привести к безопасным значениям: `max_size>=1`, `cache_kib>0`.
    /// Защищает от `pool_size=0`/`per_conn_cache_kib=0` в конфиге.
    pub fn sanitized(mut self) -> Self {
        if self.max_size == 0 {
            self.max_size = 1;
        }
        if self.cache_kib == 0 {
            self.cache_kib = DEFAULT_PER_CONN_CACHE_KIB;
        }
        self
    }
}

/// Учёт одной выдачи соединения: кто держит и с какого момента.
struct Checkout {
    /// Метка вызова — «инструмент|репо» (см. [`CHECKOUT_LABEL`]).
    label: String,
    /// Момент, когда соединение выдали.
    since: Instant,
    /// Предупреждение о затянувшейся выдаче уже записано (пишем один раз на выдачу).
    reported: bool,
}

/// Пул соединений к одной БД индекса.
pub struct StoragePool {
    /// Путь к `index.db`. `None` — режим единственного предзагруженного
    /// соединения (in-memory/тесты): новые соединения не открываются.
    db_path: Option<PathBuf>,
    /// Свободные соединения. `std::sync::Mutex` — держим микросекунды (pop/push),
    /// across-await не блокируется.
    idle: Mutex<Vec<Storage>>,
    /// Учёт выданных соединений: номер выдачи → кто и когда держит.
    checkouts: Mutex<HashMap<u64, Checkout>>,
    /// Источник номеров выдач (ключи `checkouts`).
    next_checkout: AtomicU64,
    /// Ограничивает число одновременно выданных соединений = `cfg.max_size`.
    sem: Arc<Semaphore>,
    cfg: PoolConfig,
}

impl StoragePool {
    /// Файловый пул: открывает БД read-only, прогревает одним соединением
    /// (ранняя валидация пути — как раньше при открытии единственного), остальные
    /// открываются лениво в [`get`](Self::get) по мере конкуренции.
    pub fn open_file_readonly(db_path: &Path, cfg: PoolConfig) -> Result<Arc<Self>> {
        let cfg = cfg.sanitized();
        let first = open_conn(db_path, &cfg)?;
        Ok(Arc::new(Self {
            db_path: Some(db_path.to_path_buf()),
            idle: Mutex::new(vec![first]),
            checkouts: Mutex::new(HashMap::new()),
            next_checkout: AtomicU64::new(0),
            sem: Arc::new(Semaphore::new(cfg.max_size)),
            cfg,
        }))
    }

    /// Единственное предзагруженное соединение (in-memory/тесты): `max_size=1`,
    /// новых соединений не открывает (БД может быть приватной in-memory). Принимает
    /// уже открытый `Storage` (в т.ч. read-write — для сидирования тестовых данных).
    pub fn single(storage: Storage) -> Arc<Self> {
        Arc::new(Self {
            db_path: None,
            idle: Mutex::new(vec![storage]),
            checkouts: Mutex::new(HashMap::new()),
            next_checkout: AtomicU64::new(0),
            sem: Arc::new(Semaphore::new(1)),
            cfg: PoolConfig {
                max_size: 1,
                ..PoolConfig::default()
            },
        })
    }

    /// Захватить замок списка свободных соединений.
    ///
    /// Единая политика на оба места работы со списком (взятие и возврат):
    /// отравление замка — паника другого потока, который его держал — НЕ
    /// означает порчу данных. Под замком лежит обычный список соединений, и
    /// операции над ним не оставляют его в промежуточном состоянии. Поэтому
    /// берём содержимое и работаем дальше.
    ///
    /// Раньше здесь было расхождение: взятие соединения делало `unwrap()` и
    /// роняло весь слой выдачи, а возврат по Drop отравление переживал. Одна
    /// давняя паника в стороннем потоке навсегда выводила сервер из строя.
    fn lock_idle(&self) -> std::sync::MutexGuard<'_, Vec<Storage>> {
        self.idle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Захватить замок учёта выданных соединений — той же политикой, что
    /// `lock_idle` (отравление переживаем).
    fn lock_checkouts(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Checkout>> {
        self.checkouts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// База для сообщений журнала: путь к `index.db` либо «в памяти».
    fn db_label(&self) -> String {
        match &self.db_path {
            Some(p) => p.display().to_string(),
            None => "в памяти".to_string(),
        }
    }

    /// Взять соединение. Ждёт свободный permit (одновременно не более
    /// `max_size`), переиспользует idle-соединение либо лениво открывает новое
    /// (только если задан `db_path`). Гость возвращает соединение в пул по Drop.
    pub async fn get(self: &Arc<Self>) -> Result<PooledStorage> {
        // Долгие выдачи проверяем ДО ожидания семафора: если все соединения
        // выданы и зависли, ожидание здесь бесконечно, и предупреждение должно
        // прозвучать раньше.
        self.report_long_checkouts();

        // Семафор закрывается только при остановке сервера выдачи. Это штатное
        // завершение, а не нарушенный инвариант: отдаём ошибку вызывающему,
        // паника здесь маскировала бы настоящие аварии в журнале.
        let permit = Arc::clone(&self.sem)
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("пул соединений закрыт — сервер выдачи останавливается"))?;

        let existing = self.lock_idle().pop();
        let storage = match existing {
            Some(s) => s,
            None => {
                let path = self.db_path.as_ref().expect(
                    "single-mode пул без db_path не должен открывать новые соединения \
                     (sem=1 гарантирует, что соединение всегда в idle, пока не выдано)",
                );
                open_conn(path, &self.cfg)?
            }
        };

        // Номер выдачи — ключ учёта «кто держит соединение».
        let checkout_id = self.next_checkout.fetch_add(1, Ordering::Relaxed);
        let label = CHECKOUT_LABEL
            .try_with(|l| l.clone())
            .unwrap_or_else(|_| "без метки".to_string());
        self.lock_checkouts().insert(
            checkout_id,
            Checkout {
                label,
                since: Instant::now(),
                reported: false,
            },
        );

        Ok(PooledStorage {
            storage: Some(storage),
            pool: Arc::clone(self),
            checkout_id,
            _permit: permit,
        })
    }

    /// Записать в журнал выдачи, которые держат соединение дольше
    /// [`LONG_CHECKOUT_WARN`]. Про каждую выдачу — ровно один раз, иначе журнал
    /// забьётся повторами на каждом `get`.
    fn report_long_checkouts(&self) {
        let db = self.db_label();
        let mut checkouts = self.lock_checkouts();
        for c in checkouts.values_mut() {
            if c.reported {
                continue;
            }
            let age = c.since.elapsed();
            if age < LONG_CHECKOUT_WARN {
                continue;
            }
            c.reported = true;
            tracing::warn!(
                "соединение выдано {}с назад (база: {}, метка «{}»): пока соединение держит \
                 незакрытое чтение, демон не может схлопнуть журнал WAL этой базы",
                age.as_secs(),
                db,
                c.label
            );
        }
    }

    /// Выдачи, которые держат соединение дольше `older_than`: метка и возраст.
    /// Для тестов и диагностики (в журнал пишет `report_long_checkouts`).
    pub fn long_checkouts(&self, older_than: Duration) -> Vec<(String, Duration)> {
        let checkouts = self.lock_checkouts();
        checkouts
            .values()
            .filter_map(|c| {
                let age = c.since.elapsed();
                (age >= older_than).then(|| (c.label.clone(), age))
            })
            .collect()
    }
}

/// RAII-guard: разыменовывается в [`Storage`], возвращает соединение в пул по Drop.
/// Держит owned-permit, поэтому `'static` — готов к будущему `spawn_blocking`.
pub struct PooledStorage {
    storage: Option<Storage>,
    pool: Arc<StoragePool>,
    /// Номер выдачи — ключ записи в учёте пула (`StoragePool::checkouts`).
    checkout_id: u64,
    _permit: OwnedSemaphorePermit,
}

impl std::ops::Deref for PooledStorage {
    type Target = Storage;
    fn deref(&self) -> &Storage {
        self.storage
            .as_ref()
            .expect("PooledStorage без storage — баг (использование после Drop)")
    }
}

/// Чисто ли соединение перед возвратом в пул: не в транзакции и без незакрытого
/// запроса. Всё прочее — незакрытое чтение, из-за которого демон не может
/// схлопнуть журнал WAL этой базы.
fn conn_is_clean(storage: &Storage) -> bool {
    storage.conn().is_autocommit() && !storage.conn().is_busy()
}

impl Drop for PooledStorage {
    fn drop(&mut self) {
        // Снять запись учёта и, если соединение держали дольше порога, записать
        // в журнал — именно эта строка назовёт виновника незакрытого чтения.
        let checkout = self.pool.lock_checkouts().remove(&self.checkout_id);
        let label = match checkout {
            Some(c) => {
                let held = c.since.elapsed();
                if held >= LONG_CHECKOUT_WARN {
                    tracing::warn!(
                        "затянувшаяся выдача соединения: база {}, метка «{}», держали {}с",
                        self.pool.db_label(),
                        c.label,
                        held.as_secs()
                    );
                }
                c.label
            }
            None => "без метки".to_string(),
        };

        if let Some(s) = self.storage.take() {
            // Соединение с незакрытым чтением в пул не возвращаем: оно не даст
            // демону схлопнуть журнал WAL. Сначала пробуем вылечить.
            if !conn_is_clean(&s) {
                tracing::warn!(
                    "соединение вернули с незакрытым чтением (база: {}, метка «{}») — \
                     сбрасываю кэш запросов и откатываю транзакцию",
                    self.pool.db_label(),
                    label
                );
                s.conn().flush_prepared_statement_cache();
                if !s.conn().is_autocommit() {
                    // Откат — последняя попытка вылечить соединение, а не
                    // самостоятельная операция: ошибку не поднимаем.
                    let _ = s.conn().execute_batch("ROLLBACK");
                }
            }
            if !conn_is_clean(&s) && self.pool.db_path.is_some() {
                // Файловый пул: грязное соединение НЕ возвращаем — оно закроется
                // на выходе из функции, а пул откроет новое лениво.
                tracing::warn!(
                    "соединение всё ещё держит незакрытое чтение (база: {}, метка «{}») — \
                     закрываю его, пул откроет новое",
                    self.pool.db_label(),
                    label
                );
            } else {
                // Вернуть соединение в пул. Отравление замка переживаем той же
                // политикой, что и при взятии (см. `lock_idle`): соединение
                // возвращается в список, а не теряется.
                //
                // Пул единственного соединения (`db_path` не задан) отдаёт
                // соединение как есть: выбросив его, пул опустеет навсегда.
                self.pool.lock_idle().push(s);
            }
        }
        // permit освобождается автоматически при Drop _permit.
    }
}

/// Открыть read-only соединение с настройками пула: переопределить `cache_size`
/// (initialize_readonly ставит -64000) и выставить `busy_timeout`.
fn open_conn(db_path: &Path, cfg: &PoolConfig) -> Result<Storage> {
    let storage = Storage::open_file_readonly(db_path)?;
    storage
        .conn()
        .execute_batch(&format!(
            "PRAGMA cache_size=-{}; PRAGMA busy_timeout={};",
            cfg.cache_kib, cfg.busy_timeout_ms
        ))
        .with_context(|| {
            format!(
                "PRAGMA-настройка пулового соединения: {}",
                db_path.display()
            )
        })?;
    Ok(storage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pool_reuses_and_parallelizes() {
        // Готовим файловую БД с минимальной схемой (open_file создаёт схему).
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("index.db");
        Storage::open_file(&db_path).unwrap(); // создать файл + схему

        let cfg = PoolConfig {
            max_size: 2,
            cache_kib: 4096,
            busy_timeout_ms: 1000,
        };
        let pool = StoragePool::open_file_readonly(&db_path, cfg).unwrap();

        // Два одновременных соединения берутся без взаимного ожидания.
        let a = pool.get().await.unwrap();
        let b = pool.get().await.unwrap();
        // Оба работают (простой запрос к sqlite_master).
        let _ = a.conn().execute_batch("SELECT 1;");
        let _ = b.conn().execute_batch("SELECT 1;");
        drop(a);
        drop(b);

        // После возврата соединения переиспользуются (idle не пуст).
        let c = pool.get().await.unwrap();
        let _ = c.conn().execute_batch("SELECT 1;");
    }

    /// Отравить замок списка свободных соединений: поток паникует, удерживая его.
    fn отравить_замок(pool: &Arc<StoragePool>) {
        let p = Arc::clone(pool);
        let прежний_хук = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // не засорять вывод теста
        let res = std::thread::spawn(move || {
            let _guard = p.idle.lock().unwrap();
            panic!("искусственная паника ради отравления замка");
        })
        .join();
        std::panic::set_hook(прежний_хук);
        assert!(res.is_err(), "поток должен был упасть и отравить замок");
        assert!(pool.idle.lock().is_err(), "замок должен быть отравлен");
    }

    /// Регресс G-1: взятие соединения делало `unwrap()` на отравленном замке и
    /// роняло весь слой выдачи, хотя возврат по Drop то же отравление переживал.
    /// Одна давняя паника в стороннем потоке выводила сервер из строя навсегда.
    #[tokio::test]
    async fn отравленный_замок_не_мешает_взять_соединение() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("index.db");
        Storage::open_file(&db_path).unwrap();

        let pool = StoragePool::open_file_readonly(&db_path, PoolConfig::default()).unwrap();
        отравить_замок(&pool);

        let s = pool
            .get()
            .await
            .expect("пул обязан работать на отравленном замке");
        s.conn().execute_batch("SELECT 1;").unwrap();
    }

    /// Вторая половина той же политики: соединение возвращается в список даже
    /// после отравления, а не теряется — иначе пул молча деградировал бы до
    /// открытия нового соединения на каждый запрос.
    #[tokio::test]
    async fn отравленный_замок_не_теряет_возвращаемое_соединение() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("index.db");
        Storage::open_file(&db_path).unwrap();

        let pool = StoragePool::open_file_readonly(&db_path, PoolConfig::default()).unwrap();
        let s = pool.get().await.unwrap();
        assert_eq!(pool.lock_idle().len(), 0, "соединение выдано — список пуст");

        отравить_замок(&pool);
        drop(s);

        assert_eq!(
            pool.lock_idle().len(),
            1,
            "соединение должно вернуться в список несмотря на отравление"
        );
    }

    /// Вторая паника из S-4, парная к воркеру: на закрытом семафоре пул отдаёт
    /// ошибку вызывающему, а не роняет процесс.
    #[tokio::test]
    async fn закрытый_семафор_даёт_ошибку_а_не_панику() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("index.db");
        Storage::open_file(&db_path).unwrap();

        let pool = StoragePool::open_file_readonly(&db_path, PoolConfig::default()).unwrap();
        pool.sem.close();

        // `expect_err` тут не подходит: `PooledStorage` не реализует Debug.
        let err = match pool.get().await {
            Ok(_) => panic!("на закрытом пуле ожидалась ошибка"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("закрыт"), "текст ошибки: {err}");
    }

    #[tokio::test]
    async fn sanitized_guards_zero() {
        let cfg = PoolConfig {
            max_size: 0,
            cache_kib: 0,
            busy_timeout_ms: 0,
        }
        .sanitized();
        assert_eq!(cfg.max_size, 1);
        assert_eq!(cfg.cache_kib, DEFAULT_PER_CONN_CACHE_KIB);
    }

    #[tokio::test]
    async fn single_mode_wraps_one_storage() {
        let storage = Storage::open_in_memory().unwrap();
        let pool = StoragePool::single(storage);
        let s = pool.get().await.unwrap();
        let _ = s.conn().execute_batch("SELECT 1;");
    }

    /// Главное свойство пула: при `max_size>=2` долгий «держатель» соединения
    /// НЕ блокирует второй запрос к тому же репо (раньше единственный мьютекс
    /// сериализовал — второй ждал освобождения).
    #[tokio::test]
    async fn heavy_checkout_does_not_block_other_connection() {
        use std::time::{Duration, Instant};

        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("index.db");
        Storage::open_file(&db_path).unwrap();

        let cfg = PoolConfig {
            max_size: 2,
            cache_kib: 4096,
            busy_timeout_ms: 1000,
        };
        let pool = StoragePool::open_file_readonly(&db_path, cfg).unwrap();

        // «Тяжёлый» держатель: берёт соединение и держит 300 мс.
        let held = pool.get().await.unwrap();
        let holder = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(held);
        });

        // Второй запрос получает соединение, не дожидаясь первого.
        let start = Instant::now();
        let other = pool.get().await.unwrap();
        let elapsed = start.elapsed();
        let _ = other.conn().execute_batch("SELECT 1;");
        assert!(
            elapsed < Duration::from_millis(150),
            "второй get() ждал {}мс — пул сериализует (ожидалось мгновенно)",
            elapsed.as_millis()
        );

        holder.await.unwrap();
    }

    /// Контраст: пул из ОДНОГО соединения воспроизводит прежнее поведение —
    /// второй запрос ждёт освобождения первого.
    #[tokio::test]
    async fn single_connection_pool_serializes() {
        use std::time::{Duration, Instant};

        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("index.db");
        Storage::open_file(&db_path).unwrap();

        let cfg = PoolConfig {
            max_size: 1,
            cache_kib: 4096,
            busy_timeout_ms: 1000,
        };
        let pool = StoragePool::open_file_readonly(&db_path, cfg).unwrap();

        let held = pool.get().await.unwrap();
        let holder = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(held);
        });

        let start = Instant::now();
        let other = pool.get().await.unwrap();
        let elapsed = start.elapsed();
        drop(other);
        assert!(
            elapsed >= Duration::from_millis(250),
            "второй get() при max_size=1 должен был ждать ~300мс, прошло {}мс",
            elapsed.as_millis()
        );

        holder.await.unwrap();
    }

    /// Метка выдачи: `CHECKOUT_LABEL` (её ставит `call_tool`) попадает в учёт
    /// пула, а вне области видимости выдача видна как «без метки».
    #[tokio::test]
    async fn метка_выдачи_попадает_в_учёт() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("index.db");
        Storage::open_file(&db_path).unwrap();

        let pool = StoragePool::open_file_readonly(&db_path, PoolConfig::default()).unwrap();

        CHECKOUT_LABEL
            .scope("grep_code|demo".to_string(), async {
                let s = pool.get().await.unwrap();
                let held = pool.long_checkouts(Duration::ZERO);
                assert_eq!(held.len(), 1, "выдача одна");
                assert_eq!(held[0].0, "grep_code|demo", "метка — инструмент|репо");
                drop(s);
                assert!(
                    pool.long_checkouts(Duration::ZERO).is_empty(),
                    "после освобождения учёт пуст"
                );
            })
            .await;

        // Вне области видимости метки нет — учёт показывает заглушку.
        let s = pool.get().await.unwrap();
        let held = pool.long_checkouts(Duration::ZERO);
        assert_eq!(held.len(), 1, "выдача одна");
        assert_eq!(held[0].0, "без метки");
        drop(s);
    }

    /// Незакрытая транзакция не должна уезжать обратно в пул: соединение в
    /// транзакции не даст демону схлопнуть журнал WAL.
    #[tokio::test]
    async fn транзакция_не_уезжает_в_пул() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("index.db");
        Storage::open_file(&db_path).unwrap();

        let cfg = PoolConfig {
            max_size: 1,
            cache_kib: 4096,
            busy_timeout_ms: 1000,
        };
        let pool = StoragePool::open_file_readonly(&db_path, cfg).unwrap();

        let s = pool.get().await.unwrap();
        s.conn().execute_batch("BEGIN").unwrap();
        let n: i64 = s
            .conn()
            .query_row("SELECT count(*) FROM files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n, 0, "база пуста");
        assert!(!s.conn().is_autocommit(), "внутри транзакции");
        drop(s);

        let s = pool.get().await.unwrap();
        assert!(
            s.conn().is_autocommit(),
            "соединение вернулось в пул внутри незакрытой транзакции"
        );
    }

    /// Главный сценарий утечки: запрос, возвращённый в кэш соединения
    /// несброшенным (`prepare_cached` не сбрасывает запрос), держит открытое
    /// чтение — журнал WAL базы не схлопывается.
    #[tokio::test]
    async fn несброшенный_запрос_не_держит_wal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("index.db");

        // Засеять ≥2 строки в `files` до открытия пула: запросу нужно отдать
        // строку и остаться в полёте.
        {
            let storage = Storage::open_file(&db_path).unwrap();
            for name in ["a.txt", "b.txt"] {
                storage
                    .conn()
                    .execute(
                        "INSERT INTO files (path, content_hash, language) VALUES (?1, 'h', 'text')",
                        rusqlite::params![name],
                    )
                    .unwrap();
            }
        }

        let cfg = PoolConfig {
            max_size: 1,
            cache_kib: 4096,
            busy_timeout_ms: 1000,
        };
        let pool = StoragePool::open_file_readonly(&db_path, cfg).unwrap();

        let s = pool.get().await.unwrap();
        let mut stmt = s.conn().prepare_cached("SELECT path FROM files").unwrap();
        let mut rows = stmt.query([]).unwrap();
        let first: String = rows.next().unwrap().unwrap().get(0).unwrap();
        assert!(first == "a.txt" || first == "b.txt", "прочитали: {first}");
        // Строки НЕ закрываем (`std::mem::forget` пропускает сброс запроса):
        // запрос остаётся несброшенным и уходит в кэш соединения в том же виде.
        std::mem::forget(rows);
        drop(stmt);
        drop(s);

        // Соединение вернулось в пул чистым — незакрытого чтения в нём нет.
        let s = pool.get().await.unwrap();
        assert!(
            !s.conn().is_busy(),
            "соединение вернулось в пул с несброшенным запросом"
        );
        drop(s);

        // Пишущее соединение к тому же файлу с нулевым ожиданием: checkpoint
        // проходит сразу — чтение действительно закрыто.
        let writer = rusqlite::Connection::open(&db_path).unwrap();
        writer.execute_batch("PRAGMA busy_timeout=0;").unwrap();
        let busy: i64 = writer
            .query_row("PRAGMA wal_checkpoint(TRUNCATE);", [], |row| row.get(0))
            .unwrap();
        assert_eq!(busy, 0, "журнал WAL не схлопнулся: чтение всё ещё открыто");
    }
}
