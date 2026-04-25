/// Логика выбора режима хранения: in-memory vs disk
use sysinfo::System;
use std::path::Path;

/// Режим хранения SQLite
#[derive(Debug, Clone, PartialEq)]
pub enum StorageMode {
    /// Работа в оперативной памяти (максимальная скорость)
    InMemory,
    /// Работа с файлом на диске (WAL-режим)
    Disk,
}

/// Настройки режима хранения
#[derive(Debug, Clone)]
pub struct StorageConfig {
    /// Режим: "auto" | "memory" | "disk"
    pub mode: String,
    /// Максимальный % свободной RAM, который разрешено занять под БД (по умолчанию 25)
    pub memory_max_percent: u8,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            mode: "auto".to_string(),
            memory_max_percent: 25,
        }
    }
}

/// Определить оптимальный режим хранения на основе конфига и размера БД
pub fn determine_storage_mode(config: &StorageConfig, db_path: &Path) -> StorageMode {
    match config.mode.as_str() {
        "memory" => StorageMode::InMemory,
        "disk"   => StorageMode::Disk,
        _        => auto_detect(config, db_path),
    }
}

/// Автоматическое определение: сравниваем размер БД с порогом свободной RAM
fn auto_detect(config: &StorageConfig, db_path: &Path) -> StorageMode {
    // Размер БД на диске (0 для новых баз — гарантированно поместятся в память)
    let db_size = if db_path.exists() {
        std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };

    // Читаем доступную RAM
    let mut sys = System::new();
    sys.refresh_memory();
    let available_ram = sys.available_memory(); // байты

    // Порог: memory_max_percent % свободной RAM
    let threshold = available_ram
        .saturating_mul(config.memory_max_percent as u64)
        / 100;

    if db_size <= threshold {
        StorageMode::InMemory
    } else {
        StorageMode::Disk
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_determine_storage_mode_force_memory() {
        let config = StorageConfig {
            mode: "memory".to_string(),
            memory_max_percent: 25,
        };
        let mode = determine_storage_mode(&config, Path::new("/nonexistent/db"));
        assert_eq!(mode, StorageMode::InMemory);
    }

    #[test]
    fn test_determine_storage_mode_force_disk() {
        let config = StorageConfig {
            mode: "disk".to_string(),
            memory_max_percent: 25,
        };
        let mode = determine_storage_mode(&config, Path::new("/nonexistent/db"));
        assert_eq!(mode, StorageMode::Disk);
    }

    #[test]
    fn test_determine_storage_mode_auto_new_db() {
        // Новая БД (файл не существует) — размер 0, всегда помещается в память
        let config = StorageConfig::default();
        let mode = determine_storage_mode(&config, Path::new("/nonexistent/index.db"));
        assert_eq!(mode, StorageMode::InMemory);
    }
}
