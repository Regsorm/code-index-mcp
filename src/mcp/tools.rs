/// Реализации инструментов MCP-сервера
///
/// Каждая функция блокирует мьютекс Storage, вызывает нужный метод,
/// сериализует результат в JSON и возвращает строку.
use super::CodeIndexServer;

/// Вспомогательная функция: сериализует значение в JSON или возвращает ошибку
fn to_json<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_string_pretty(value) {
        Ok(s) => s,
        Err(e) => format!("{{\"error\": \"Ошибка сериализации: {}\"}}", e),
    }
}

/// FTS-поиск функций по запросу
pub async fn search_function(server: &CodeIndexServer, query: String, limit: Option<usize>, language: Option<String>) -> String {
    let storage = server.storage.lock().await;
    match storage.search_functions(&query, limit.unwrap_or(20), language.as_deref()) {
        Ok(results) => to_json(&results),
        Err(e) => format!("{{\"error\": \"search_function: {}\"}}", e),
    }
}

/// FTS-поиск классов по запросу
pub async fn search_class(server: &CodeIndexServer, query: String, limit: Option<usize>, language: Option<String>) -> String {
    let storage = server.storage.lock().await;
    match storage.search_classes(&query, limit.unwrap_or(20), language.as_deref()) {
        Ok(results) => to_json(&results),
        Err(e) => format!("{{\"error\": \"search_class: {}\"}}", e),
    }
}

/// Поиск функции по точному имени
pub async fn get_function(server: &CodeIndexServer, name: String) -> String {
    let storage = server.storage.lock().await;
    match storage.get_function_by_name(&name) {
        Ok(results) => to_json(&results),
        Err(e) => format!("{{\"error\": \"get_function: {}\"}}", e),
    }
}

/// Поиск класса по точному имени
pub async fn get_class(server: &CodeIndexServer, name: String) -> String {
    let storage = server.storage.lock().await;
    match storage.get_class_by_name(&name) {
        Ok(results) => to_json(&results),
        Err(e) => format!("{{\"error\": \"get_class: {}\"}}", e),
    }
}

/// Кто вызывает данную функцию
pub async fn get_callers(server: &CodeIndexServer, function_name: String, language: Option<String>) -> String {
    let storage = server.storage.lock().await;
    match storage.get_callers(&function_name, language.as_deref()) {
        Ok(results) => to_json(&results),
        Err(e) => format!("{{\"error\": \"get_callers: {}\"}}", e),
    }
}

/// Что вызывает данная функция
pub async fn get_callees(server: &CodeIndexServer, function_name: String, language: Option<String>) -> String {
    let storage = server.storage.lock().await;
    match storage.get_callees(&function_name, language.as_deref()) {
        Ok(results) => to_json(&results),
        Err(e) => format!("{{\"error\": \"get_callees: {}\"}}", e),
    }
}

/// Универсальный поиск символа (функции + классы + переменные + импорты)
pub async fn find_symbol(server: &CodeIndexServer, name: String, language: Option<String>) -> String {
    let storage = server.storage.lock().await;
    match storage.find_symbol(&name, language.as_deref()) {
        Ok(result) => to_json(&result),
        Err(e) => format!("{{\"error\": \"find_symbol: {}\"}}", e),
    }
}

/// Импорты по file_id или по имени модуля
pub async fn get_imports(
    server: &CodeIndexServer,
    file_id: Option<i64>,
    module: Option<String>,
    language: Option<String>,
) -> String {
    let storage = server.storage.lock().await;
    // Если задан file_id — поиск по файлу (language не применяется, file_id уникален)
    if let Some(fid) = file_id {
        return match storage.get_imports_by_file(fid) {
            Ok(results) => to_json(&results),
            Err(e) => format!("{{\"error\": \"get_imports_by_file: {}\"}}", e),
        };
    }
    // Если задан модуль — поиск по модулю с необязательным фильтром по языку
    if let Some(ref m) = module {
        return match storage.get_imports_by_module(m, language.as_deref()) {
            Ok(results) => to_json(&results),
            Err(e) => format!("{{\"error\": \"get_imports_by_module: {}\"}}", e),
        };
    }
    // Ни один из параметров не задан
    "{\"error\": \"Необходимо указать file_id или module\"}".to_string()
}

/// Сводная карта файла
pub async fn get_file_summary(server: &CodeIndexServer, path: String) -> String {
    let storage = server.storage.lock().await;
    match storage.get_file_summary(&path) {
        Ok(Some(summary)) => to_json(&summary),
        Ok(None) => format!("{{\"error\": \"Файл '{}' не найден в индексе\"}}", path),
        Err(e) => format!("{{\"error\": \"get_file_summary: {}\"}}", e),
    }
}

/// Статистика базы данных + статус индексации
pub async fn get_stats(server: &CodeIndexServer) -> String {
    let storage = server.storage.lock().await;
    match storage.get_stats() {
        Ok(mut stats) => {
            // Подставляем статус индексации из shared state
            let status = server.indexing_status.lock().await;
            stats.indexing_status = Some(status.clone());
            to_json(&stats)
        }
        Err(e) => format!("{{\"error\": \"get_stats: {}\"}}", e),
    }
}

/// Поиск подстроки или regex в телах функций и классов
pub async fn grep_body(
    server: &CodeIndexServer,
    pattern: Option<String>,
    regex: Option<String>,
    language: Option<String>,
    limit: Option<usize>,
) -> String {
    let storage = server.storage.lock().await;
    match storage.grep_body(
        pattern.as_deref(),
        regex.as_deref(),
        language.as_deref(),
        limit.unwrap_or(100),
    ) {
        Ok(results) => to_json(&results),
        Err(e) => format!("{{\"error\": \"grep_body: {}\"}}", e),
    }
}

/// FTS-поиск по текстовым файлам
pub async fn search_text(server: &CodeIndexServer, query: String, limit: Option<usize>, language: Option<String>) -> String {
    let storage = server.storage.lock().await;
    match storage.search_text(&query, limit.unwrap_or(20), language.as_deref()) {
        Ok(results) => {
            // Преобразуем Vec<(String, String)> в массив объектов для удобства
            let items: Vec<serde_json::Value> = results
                .into_iter()
                .map(|(path, snippet)| {
                    serde_json::json!({
                        "path": path,
                        "snippet": snippet
                    })
                })
                .collect();
            to_json(&items)
        }
        Err(e) => format!("{{\"error\": \"search_text: {}\"}}", e),
    }
}
