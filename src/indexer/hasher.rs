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
pub fn file_hash(path: &Path) -> Result<(String, String)> {
    let bytes = std::fs::read(path)?;
    let hash = content_hash(&bytes);
    // Потерянные байты заменяем — лучше частичный текст, чем ошибка
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok((text, hash))
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
}
