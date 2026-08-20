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
    /// Максимальный % свободной RAM, который разрешено занять под БД (по умолчанию 50)
    pub memory_max_percent: u8,
    /// Во сколько байт обойдётся работа с папкой, если вести её в памяти.
    /// Для НОВОЙ базы размер файла на диске равен нулю и ничего не говорит,
    /// поэтому расход оценивается заранее — по весу исходников (см. вызов
    /// в воркере демона). Ноль означает «оценки нет».
    pub expected_bytes: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            mode: "auto".to_string(),
            memory_max_percent: 50,
            expected_bytes: 0,
        }
    }
}

/// Попросить распределитель вернуть системе память, которую программа уже
/// освободила.
///
/// Зачем: стандартный распределитель glibc, освободив крупные куски, держит их
/// в куче процесса и системе не отдаёт. После работы с базой в памяти это
/// заметно: замер на узле — шесть папок подряд, занято 9,8 → 27,8 → 41,8 →
/// 53,2 ГБ, хотя каждая база к тому моменту уже была сброшена на диск и
/// закрыта. Следующей папке эта память видна как занятая, и она уходит на
/// диск, даже когда машина её потянула бы.
///
/// Возвращает `true`, если распределитель сообщил, что память отдана. На
/// Windows и macOS ничего не делает: там у кучи своё поведение и такой ручки
/// нет — возвращает `false`.
pub fn release_free_memory() -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: malloc_trim не принимает и не возвращает указателей, работает
        // с внутренним состоянием распределителя. Аргумент — сколько байт
        // оставить про запас на вершине кучи; 0 — не оставлять ничего.
        unsafe { libc::malloc_trim(0) == 1 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Сколько оперативной памяти сейчас свободно, в байтах.
///
/// Читается заново на каждую папку — и это важно: если предыдущая папка память
/// не вернула, следующая увидит меньше свободной и сама уйдёт на диск.
pub fn available_ram() -> u64 {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.available_memory()
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

    // Порог: memory_max_percent % свободной RAM
    let threshold = available_ram()
        .saturating_mul(config.memory_max_percent as u64)
        / 100;

    // Во что обойдётся работа в памяти. У существующей базы это её размер на
    // диске, у новой — оценка по весу исходников: ноль байт файла ничего не
    // говорит о том, во что папка развернётся после индексации. Берём большее
    // из двух: оценка может прийти и к уже существующей базе.
    let expected = db_size.max(config.expected_bytes);

    // Оценки нет и базы нет — брать в память нечего мерить, идём на диск.
    // Так безопаснее: прежде такая папка уходила в память по формальному
    // «ноль меньше порога», и на узле с шестью крупными папками подряд память
    // не возвращалась от папки к папке (замер: 9,8 → 27,8 → 41,8 → 53,2 ГБ).
    if expected == 0 {
        return StorageMode::Disk;
    }

    if expected <= threshold {
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
            expected_bytes: 0,
        };
        let mode = determine_storage_mode(&config, Path::new("/nonexistent/db"));
        assert_eq!(mode, StorageMode::InMemory);
    }

    #[test]
    fn test_determine_storage_mode_force_disk() {
        let config = StorageConfig {
            mode: "disk".to_string(),
            memory_max_percent: 25,
            expected_bytes: 0,
        };
        let mode = determine_storage_mode(&config, Path::new("/nonexistent/db"));
        assert_eq!(mode, StorageMode::Disk);
    }

    #[test]
    fn новая_база_без_оценки_идёт_на_диск() {
        // Базы нет, оценку никто не передал — судить не по чему. Прежде такая
        // папка уходила в память по формальному «ноль меньше порога».
        let config = StorageConfig::default();
        let mode = determine_storage_mode(&config, Path::new("/nonexistent/index.db"));
        assert_eq!(mode, StorageMode::Disk);
    }

    #[test]
    fn новая_база_решается_по_оценке_а_не_по_размеру_файла() {
        // Оценка заведомо неподъёмная: больше всей памяти машины.
        let too_big = StorageConfig {
            mode: "auto".to_string(),
            memory_max_percent: 50,
            expected_bytes: available_ram().saturating_mul(4).max(u64::from(u32::MAX)),
        };
        assert_eq!(
            determine_storage_mode(&too_big, Path::new("/nonexistent/index.db")),
            StorageMode::Disk,
            "папка не влезает в разрешённую долю памяти"
        );

        // Оценка заведомо скромная — один мегабайт.
        let small = StorageConfig {
            mode: "auto".to_string(),
            memory_max_percent: 50,
            expected_bytes: 1024 * 1024,
        };
        assert_eq!(
            determine_storage_mode(&small, Path::new("/nonexistent/index.db")),
            StorageMode::InMemory,
            "мегабайт влезает в половину свободной памяти на любой машине"
        );
    }

    #[test]
    fn явная_настройка_памяти_сильнее_правила_о_новой_базе() {
        let config = StorageConfig {
            mode: "memory".to_string(),
            memory_max_percent: 25,
            expected_bytes: 0,
        };
        let mode = determine_storage_mode(&config, Path::new("/nonexistent/index.db"));
        assert_eq!(mode, StorageMode::InMemory, "оператор попросил память явно");
    }
}
