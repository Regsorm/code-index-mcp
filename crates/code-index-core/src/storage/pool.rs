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

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
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

/// Пул соединений к одной БД индекса.
pub struct StoragePool {
    /// Путь к `index.db`. `None` — режим единственного предзагруженного
    /// соединения (in-memory/тесты): новые соединения не открываются.
    db_path: Option<PathBuf>,
    /// Свободные соединения. `std::sync::Mutex` — держим микросекунды (pop/push),
    /// across-await не блокируется.
    idle: Mutex<Vec<Storage>>,
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
            sem: Arc::new(Semaphore::new(1)),
            cfg: PoolConfig {
                max_size: 1,
                ..PoolConfig::default()
            },
        })
    }

    /// Взять соединение. Ждёт свободный permit (одновременно не более
    /// `max_size`), переиспользует idle-соединение либо лениво открывает новое
    /// (только если задан `db_path`). Гость возвращает соединение в пул по Drop.
    pub async fn get(self: &Arc<Self>) -> Result<PooledStorage> {
        let permit = Arc::clone(&self.sem)
            .acquire_owned()
            .await
            .expect("StoragePool semaphore закрыт — этого не должно происходить");

        let existing = self.idle.lock().unwrap().pop();
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

        Ok(PooledStorage {
            storage: Some(storage),
            pool: Arc::clone(self),
            _permit: permit,
        })
    }
}

/// RAII-guard: разыменовывается в [`Storage`], возвращает соединение в пул по Drop.
/// Держит owned-permit, поэтому `'static` — готов к будущему `spawn_blocking`.
pub struct PooledStorage {
    storage: Option<Storage>,
    pool: Arc<StoragePool>,
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

impl Drop for PooledStorage {
    fn drop(&mut self) {
        if let Some(s) = self.storage.take() {
            // Вернуть соединение в пул. Если Mutex отравлен — просто роняем
            // соединение (оно закроется), не паникуем повторно.
            if let Ok(mut idle) = self.pool.idle.lock() {
                idle.push(s);
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
}
