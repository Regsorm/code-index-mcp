//! In-process кэш результатов tool-вызовов для `serve`.
//!
//! Это встроенная форма бывшего внешнего прокси `mcp-cache-ci` для ci-цепочки:
//! тот же механизм (кэш ответов + инвалидация по событию переиндексации), но
//! внутри процесса serve, без сетевого хопа и без mtime-сверки сетевой гонки.
//! Кэшируется сериализованный `CallToolResult` по ключу
//! `{scope}|{tool}|{sha256_hex(args_без_repo)}`, где `scope` = значение `repo`.
//!
//! Свежесть и инвалидация — ПО ФАЙЛУ, без огрубления на весь репо. Демон шлёт
//! `/mark-dirty {files:[{path, mtime}]}` (файл изменён на диске) и post-commit
//! `/invalidate {file_paths}` (индекс догнал). Ответ не кэшируется/не отдаётся
//! из кэша, только если его файл-источник «грязный» (observed-mtime новее
//! index_mtime из `_meta.file_mtimes` ответа); инвалидация сносит только ключи,
//! зависящие от изменённого файла (обратный индекс `reverse`). Запросы про не
//! тронутые файлы не страдают. `invalidate_scope`/`invalidate_all` — для
//! полного/репо-сброса (force-reindex). TTL — подстраховка от пропущенного сигнала.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Запись кэша: сериализованный payload + момент истечения TTL.
struct Entry {
    payload: Arc<String>,
    expires: Instant,
}

/// In-memory кэш результатов tool-вызовов, общий на все сессии serve.
pub struct ServeCache {
    store: RwLock<HashMap<String, Entry>>,
    /// Файлы, изменённые на диске (пришёл `/mark-dirty` с observed-mtime), но
    /// ещё не догнанные индексом. Ключ — `(repo, rel_path)`, значение —
    /// `(observed_mtime, момент пометки)`. Используется ПО ФАЙЛУ: ответ не
    /// кэшируется и не отдаётся из кэша, если его файл-источник «грязный»
    /// (observed_mtime новее index_mtime из `_meta.file_mtimes` ответа). Без
    /// огрубления на весь репо — страдают только запросы, зависящие от файла.
    dirty: RwLock<HashMap<(String, String), (i64, Instant)>>,
    /// Обратный индекс «(repo, файл) → ключи кэша, зависящие от него» (из
    /// `_meta.dependent_files` при `insert`). Позволяет инвалидировать ТОЛЬКО
    /// зависящие от изменённого файла ключи, а не весь репо.
    reverse: RwLock<HashMap<(String, String), HashSet<String>>>,
    ttl: Duration,
    /// Страховка: «грязная» пометка файла протухает через этот срок, если
    /// post-commit `/invalidate` не пришёл (демон упал в переразборе).
    dirty_max_age: Duration,
    enabled: bool,
    hits: AtomicU64,
    misses: AtomicU64,
    invalidations: AtomicU64,
}

impl ServeCache {
    /// `ttl_secs` — время жизни записи (подстраховка к инвалидации; 0 → минимум 1с).
    /// `enabled=false` — кэш-пустышка (get всегда промах, insert — no-op).
    pub fn new(ttl_secs: u64, enabled: bool) -> Self {
        Self {
            store: RwLock::new(HashMap::new()),
            dirty: RwLock::new(HashMap::new()),
            reverse: RwLock::new(HashMap::new()),
            ttl: Duration::from_secs(ttl_secs.max(1)),
            // Страховка: если post-commit `/invalidate` не пришёл (демон упал в
            // переразборе), «грязная» пометка файла протухает через 120с.
            dirty_max_age: Duration::from_secs(120),
            enabled,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            invalidations: AtomicU64::new(0),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Построить ключ `{scope}|{tool}|{sha256_hex(args)}`. `repo` исключается из
    /// хэша (он и есть `scope`-префикс), остальные аргументы нормализуются
    /// (рекурсивная сортировка ключей объектов) для стабильности хэша.
    pub fn key(scope: &str, tool: &str, args: &Value) -> String {
        let mut stripped = args.clone();
        if let Value::Object(map) = &mut stripped {
            map.remove("repo");
        }
        let normalized = sort_keys(stripped);
        let serialized = serde_json::to_string(&normalized).unwrap_or_else(|_| "null".into());
        let mut hasher = Sha256::new();
        hasher.update(serialized.as_bytes());
        let hash = hex::encode(hasher.finalize());
        format!("{scope}|{tool}|{hash}")
    }

    /// Достать payload по ключу. Истёкшую по TTL запись считаем промахом
    /// (ленивая чистка — удалится при следующем insert/invalidate).
    pub fn get(&self, key: &str) -> Option<Arc<String>> {
        if !self.enabled {
            return None;
        }
        let guard = self.store.read().unwrap();
        match guard.get(key) {
            Some(e) if e.expires > Instant::now() => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(e.payload.clone())
            }
            _ => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Положить payload по ключу (с TTL от now) и зарегистрировать зависимость
    /// от файлов `deps` (rel_path) в обратном индексе — для per-file инвалидации.
    /// No-op при `enabled=false`.
    pub fn insert(&self, key: String, payload: Arc<String>, repo: &str, deps: &[String]) {
        if !self.enabled {
            return;
        }
        let entry = Entry {
            payload,
            expires: Instant::now() + self.ttl,
        };
        self.store.write().unwrap().insert(key.clone(), entry);
        if !deps.is_empty() {
            let mut rev = self.reverse.write().unwrap();
            for d in deps {
                rev.entry((repo.to_string(), d.clone()))
                    .or_default()
                    .insert(key.clone());
            }
        }
    }

    /// Снести все ключи репо (`scope|...`). Возвращает число удалённых записей.
    /// Вызывается при переиндексации репо (сигнал watcher → serve).
    pub fn invalidate_scope(&self, scope: &str) -> usize {
        if !self.enabled {
            return 0;
        }
        let prefix = format!("{scope}|");
        let mut guard = self.store.write().unwrap();
        let before = guard.len();
        guard.retain(|k, _| !k.starts_with(&prefix));
        let removed = before - guard.len();
        drop(guard);
        // Обратный индекс этого репо больше не нужен.
        self.reverse.write().unwrap().retain(|(r, _), _| r != scope);
        if removed > 0 {
            self.invalidations.fetch_add(removed as u64, Ordering::Relaxed);
        }
        removed
    }

    /// Полный сброс кэша (на случай глобальной переиндексации/ребилда).
    pub fn invalidate_all(&self) -> usize {
        if !self.enabled {
            return 0;
        }
        let mut guard = self.store.write().unwrap();
        let removed = guard.len();
        guard.clear();
        drop(guard);
        self.reverse.write().unwrap().clear();
        self.dirty.write().unwrap().clear();
        self.invalidations.fetch_add(removed as u64, Ordering::Relaxed);
        removed
    }

    /// Пометить файлы «грязными» (пришёл `/mark-dirty` с observed-mtime диска):
    /// файлы изменены, индекс ещё не догнал. `files` — `(rel_path, observed_mtime)`.
    pub fn mark_dirty(&self, repo: &str, files: &[(String, i64)]) {
        if !self.enabled || files.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut d = self.dirty.write().unwrap();
        for (path, mtime) in files {
            d.insert((repo.to_string(), path.clone()), (*mtime, now));
        }
    }

    /// Файл «грязный» относительно `index_mtime` (из `_meta.file_mtimes` ответа)?
    /// `true` → на диске версия новее, чем в индексе (ответ построен на не
    /// догнавшем индексе → не кэшировать/не отдавать из кэша). Протухшую по
    /// `dirty_max_age` пометку лениво снимаем.
    pub fn is_path_stale(&self, repo: &str, path: &str, index_mtime: i64) -> bool {
        if !self.enabled {
            return false;
        }
        let key = (repo.to_string(), path.to_string());
        {
            let guard = self.dirty.read().unwrap();
            match guard.get(&key) {
                None => return false,
                Some((observed, marked)) if marked.elapsed() < self.dirty_max_age => {
                    return *observed > index_mtime;
                }
                Some(_) => {} // протухло — снимем ниже
            }
        }
        self.dirty.write().unwrap().remove(&key);
        false
    }

    /// Per-file инвалидация (post-commit `/invalidate {file_paths}`): для каждого
    /// файла снять «грязную» пометку и снести из кэша ТОЛЬКО ключи, зависящие от
    /// этого файла (через обратный индекс) — без огрубления на весь репо.
    /// Возвращает число снесённых записей кэша.
    pub fn invalidate_files(&self, repo: &str, paths: &[String]) -> usize {
        if !self.enabled || paths.is_empty() {
            return 0;
        }
        // 1) снять «грязные» пометки этих файлов.
        {
            let mut d = self.dirty.write().unwrap();
            for p in paths {
                d.remove(&(repo.to_string(), p.clone()));
            }
        }
        // 2) собрать зависящие от файлов ключи и почистить обратный индекс.
        let mut keys: HashSet<String> = HashSet::new();
        {
            let mut rev = self.reverse.write().unwrap();
            for p in paths {
                if let Some(set) = rev.remove(&(repo.to_string(), p.clone())) {
                    keys.extend(set);
                }
            }
        }
        if keys.is_empty() {
            return 0;
        }
        // 3) снести эти ключи из store.
        let mut store = self.store.write().unwrap();
        let mut removed = 0usize;
        for k in &keys {
            if store.remove(k).is_some() {
                removed += 1;
            }
        }
        if removed > 0 {
            self.invalidations
                .fetch_add(removed as u64, Ordering::Relaxed);
        }
        removed
    }

    /// Число «грязных» файлов сейчас (для /cache-stats).
    pub fn dirty_count(&self) -> usize {
        self.dirty.read().unwrap().len()
    }

    /// Снимок счётчиков для /health: (entries, hits, misses, invalidations).
    pub fn stats(&self) -> (usize, u64, u64, u64) {
        (
            self.store.read().unwrap().len(),
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
            self.invalidations.load(Ordering::Relaxed),
        )
    }
}

/// Рекурсивная сортировка ключей JSON-объектов — стабильная сериализация
/// (мирроринг `cache-core::normalize_args`: одинаковые args → одинаковый хэш
/// независимо от порядка ключей у клиента).
fn sort_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> =
                map.into_iter().map(|(k, v)| (k, sort_keys(v))).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut sorted = serde_json::Map::with_capacity(entries.len());
            for (k, v) in entries {
                sorted.insert(k, v);
            }
            Value::Object(sorted)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_is_stable_regardless_of_arg_order() {
        let a = json!({"repo": "ut", "name": "X", "language": "bsl"});
        let b = json!({"language": "bsl", "name": "X", "repo": "ut"});
        assert_eq!(
            ServeCache::key("ut", "get_function", &a),
            ServeCache::key("ut", "get_function", &b)
        );
    }

    #[test]
    fn key_excludes_repo_from_hash_but_keeps_scope_prefix() {
        let args = json!({"repo": "ut", "name": "X"});
        let k_ut = ServeCache::key("ut", "get_function", &args);
        let k_bp = ServeCache::key("bp", "get_function", &json!({"repo": "bp", "name": "X"}));
        // префиксы разные (scope), хвост-хэш одинаковый (args без repo совпадают)
        assert!(k_ut.starts_with("ut|get_function|"));
        assert!(k_bp.starts_with("bp|get_function|"));
        let tail_ut = k_ut.rsplit('|').next().unwrap();
        let tail_bp = k_bp.rsplit('|').next().unwrap();
        assert_eq!(tail_ut, tail_bp);
    }

    #[test]
    fn get_insert_roundtrip() {
        let c = ServeCache::new(60, true);
        let key = ServeCache::key("ut", "get_function", &json!({"name": "X"}));
        assert!(c.get(&key).is_none());
        c.insert(key.clone(), Arc::new("payload".into()), "ut", &[]);
        assert_eq!(c.get(&key).as_deref().map(String::as_str), Some("payload"));
    }

    #[test]
    fn invalidate_scope_drops_only_that_repo() {
        let c = ServeCache::new(60, true);
        let k_ut = ServeCache::key("ut", "grep_code", &json!({"q": "x"}));
        let k_bp = ServeCache::key("bp", "grep_code", &json!({"q": "x"}));
        c.insert(k_ut.clone(), Arc::new("a".into()), "ut", &[]);
        c.insert(k_bp.clone(), Arc::new("b".into()), "bp", &[]);
        assert_eq!(c.invalidate_scope("ut"), 1);
        assert!(c.get(&k_ut).is_none());
        assert!(c.get(&k_bp).is_some());
    }

    #[test]
    fn dirty_marks_and_per_file_invalidation() {
        let c = ServeCache::new(60, true);
        // Файл X помечен грязным с observed_mtime=200.
        c.mark_dirty("ut", &[("src/X.bsl".to_string(), 200)]);
        // index_mtime=100 < 200 → ответ по X устарел.
        assert!(c.is_path_stale("ut", "src/X.bsl", 100));
        // index догнал (index_mtime >= observed) → не устарел.
        assert!(!c.is_path_stale("ut", "src/X.bsl", 200));
        // Другой файл не помечен → не устарел (нет огрубления на репо).
        assert!(!c.is_path_stale("ut", "src/Y.bsl", 1));
        assert!(!c.is_path_stale("bp", "src/X.bsl", 1)); // изоляция по репо

        // Кэш: ключ k_x зависит от X, k_y — от Y.
        let k_x = ServeCache::key("ut", "get_function", &json!({"name": "x"}));
        let k_y = ServeCache::key("ut", "get_function", &json!({"name": "y"}));
        c.insert(k_x.clone(), Arc::new("a".into()), "ut", &["src/X.bsl".to_string()]);
        c.insert(k_y.clone(), Arc::new("b".into()), "ut", &["src/Y.bsl".to_string()]);
        // Инвалидация по X сносит только ключ X, не Y.
        assert_eq!(c.invalidate_files("ut", &["src/X.bsl".to_string()]), 1);
        assert!(c.get(&k_x).is_none());
        assert!(c.get(&k_y).is_some());
        // Пометка X снята.
        assert!(!c.is_path_stale("ut", "src/X.bsl", 100));
    }

    #[test]
    fn disabled_cache_is_noop() {
        let c = ServeCache::new(60, false);
        let key = ServeCache::key("ut", "t", &json!({}));
        c.insert(key.clone(), Arc::new("x".into()), "ut", &[]);
        assert!(c.get(&key).is_none());
    }
}
