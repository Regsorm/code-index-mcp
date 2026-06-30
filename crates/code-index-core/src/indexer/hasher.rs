use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::Path;

/// Вычислить SHA-256 хеш содержимого байт и вернуть hex-строку
pub fn content_hash(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    hex::encode(hasher.finalize())
}

/// Прочитать файл с диска и вычислить его SHA-256 хеш.
/// Возвращает кортеж (содержимое как строка, hex-хеш).
/// Не-UTF8 байты заменяются символом замены U+FFFD.
pub fn file_hash(path: &Path) -> Result<(String, String, bool)> {
    let bytes = std::fs::read(path)?;
    let hash = content_hash(&bytes);
    let is_binary = looks_binary(&bytes);
    // Потерянные байты заменяем — лучше частичный текст, чем ошибка
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok((text, hash, is_binary))
}

/// Двоичный ли контент. Маркеры: сигнатура контейнера 1С `FF FF FF 7F`
/// (защищённые модули поставщика — EDT выгружает их как `.bsl` с двоичным
/// образом вместо текста, конфигуратор для них использует `.bin`) либо
/// NUL-байт в первых килобайтах (классический признак не-текста). Такой
/// контент нельзя отдавать в tree-sitter: на бесструктурном вводе парсер
/// деградирует квадратично и вешает индексацию (один файл 1.3 МБ — десятки
/// минут на одном ядре). Проверено на ~81k .bsl (5 конфигураций, 2 формата
/// выгрузки): 0 ложных срабатываний — реальные исходники только UTF-8,
/// NUL не содержат.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xFF, 0xFF, 0x7F])
        || bytes.iter().take(8192).any(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_hash_deterministic() {
        let h1 = content_hash(b"hello world");
        let h2 = content_hash(b"hello world");
        assert_eq!(h1, h2, "хеш должен быть детерминированным");
    }

    #[test]
    fn test_content_hash_differs() {
        let h1 = content_hash(b"hello world");
        let h2 = content_hash(b"different content");
        assert_ne!(h1, h2, "разные данные — разные хеши");
    }

    #[test]
    fn test_content_hash_known_value() {
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let empty_hash = content_hash(b"");
        assert_eq!(
            empty_hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_looks_binary() {
        // Сигнатура контейнера 1С (защищённый модуль, EDT кладёт его в .bsl)
        assert!(looks_binary(&[0xFF, 0xFF, 0xFF, 0x7F, 0x00, 0x02]));
        // NUL-байт в первых килобайтах — признак не-текста
        assert!(looks_binary(b"prefix\0suffix"));
        // Чистый BSL-исходник (UTF-8) — не двоичный
        assert!(!looks_binary(
            "Процедура Тест() Экспорт\nКонецПроцедуры".as_bytes()
        ));
        // Пустой файл — не двоичный
        assert!(!looks_binary(b""));
    }
}
