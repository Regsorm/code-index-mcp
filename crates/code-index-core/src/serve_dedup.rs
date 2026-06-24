//! Сессионный дедуп РЕ-ДОСТАВКИ результатов tool-вызовов.
//!
//! Идея: в рамках ОДНОЙ сессии не отдавать повторно строки результата, уже
//! доставленные ранее в этой же сессии (модель их уже видела в своём контексте).
//! Ключ — session id (из заголовка `mcp-session-id`, см. `call_tool`).
//!
//! Гранулярность — ЭЛЕМЕНТ (строка табличного результата). Замер показал, что
//! единственный заметный источник перекрытия — повторные строки табличных
//! ответов (`bsl_sql` и пр., форма `{result:{rows:[...]}}`); тела файлов и
//! идентичные ответы целиком в сессии почти не повторяются. Поэтому дедуп
//! применяется ТОЛЬКО к табличной форме; всё остальное проходит без изменений
//! (консервативно — не трогаем то, что не умеем безопасно переписать).
//!
//! Маркер вместо тишины: опущенные строки заменяются НЕ молча, а полем
//! `rows_elided_already_delivered: N` в `result`, чтобы модель понимала, что N
//! строк уже отдавались в этой сессии (и при необходимости нашла их в контексте).
//! Это correctness-sensitive (в отличие от прозрачного кэша) — потому маркер явный.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

pub struct SessionDedup {
    enabled: bool,
    /// session_id → множество усечённых хэшей уже отданных строк.
    sessions: RwLock<HashMap<String, HashSet<u64>>>,
    /// Потолок строк на сессию (защита памяти). При превышении — перестаём
    /// запоминать новые (дедуп деградирует, но не течёт). 50k×8б ≈ 400КБ/сессия.
    max_rows_per_session: usize,
    /// Потолок числа сессий в памяти (защита от утечки за дни работы: каждая
    /// новая сессия агента добавляет запись). При превышении карта целиком
    /// очищается — дедуп сбрасывается (строки разок переотдадутся, корректность
    /// не страдает), память ограничена.
    max_sessions: usize,
    elided_total: AtomicU64,
}

impl SessionDedup {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            sessions: RwLock::new(HashMap::new()),
            max_rows_per_session: 50_000,
            max_sessions: 2_000,
            elided_total: AtomicU64::new(0),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Забыть состояние сессии (на закрытии сессии). Без вызова — подчистится
    /// при рестарте serve; одна сессия ограничена `max_rows_per_session`.
    pub fn forget(&self, session_id: &str) {
        self.sessions.write().unwrap().remove(session_id);
    }

    /// (сессий в памяти, всего опущено строк) — для /cache-stats.
    pub fn stats(&self) -> (usize, u64) {
        (
            self.sessions.read().unwrap().len(),
            self.elided_total.load(Ordering::Relaxed),
        )
    }

    /// Обработать сериализованный `CallToolResult` (JSON-строку): опустить строки
    /// табличного результата, уже отданные в этой сессии, и запомнить новые.
    /// Возвращает (возможно переписанный payload, число опущенных строк).
    /// Если форма не табличная / session_id нет / дедуп выключен — payload без
    /// изменений и 0.
    pub fn process(&self, session_id: Option<&str>, payload: &str) -> (String, usize) {
        if !self.enabled {
            return (payload.to_string(), 0);
        }
        let Some(sid) = session_id else {
            return (payload.to_string(), 0);
        };
        let Ok(mut outer) = serde_json::from_str::<Value>(payload) else {
            return (payload.to_string(), 0);
        };

        // Данные tool'а лежат в MCP CallToolResult: content[*].text — это
        // вложенная JSON-строка `{result, _meta}`. Находим её, дедупим, кладём
        // обратно. structuredContent (если есть) дублирует — обновляем и его.
        let mut elided = 0usize;

        // 1) content[0].text (вложенный JSON-string)
        if let Some(text_idx) = find_text_content_index(&outer) {
            if let Some(text) = outer["content"][text_idx]["text"].as_str() {
                if let Ok(mut inner) = serde_json::from_str::<Value>(text) {
                    elided += self.dedup_rows_in_result(sid, &mut inner);
                    if elided > 0 {
                        if let Ok(s) = serde_json::to_string(&inner) {
                            outer["content"][text_idx]["text"] = Value::String(s);
                        }
                    }
                }
            }
        }

        // 2) structuredContent (rmcp structured output, дублирует данные)
        if outer.get("structuredContent").is_some() {
            let mut sc = outer["structuredContent"].take();
            let e2 = self.dedup_rows_in_result(sid, &mut sc);
            outer["structuredContent"] = sc;
            // structuredContent дублирует content[0].text — НЕ суммируем в elided,
            // считаем по первому источнику; но переписать обязаны для консистентности.
            let _ = e2;
        }

        if elided == 0 {
            return (payload.to_string(), 0);
        }
        self.elided_total.fetch_add(elided as u64, Ordering::Relaxed);
        match serde_json::to_string(&outer) {
            Ok(s) => (s, elided),
            Err(_) => (payload.to_string(), 0),
        }
    }

    /// Найти `result.rows` (или `result` как массив) в объекте `{result, _meta}`,
    /// опустить уже отданные строки, добавить маркер. Возвращает число опущенных.
    fn dedup_rows_in_result(&self, sid: &str, obj: &mut Value) -> usize {
        // result.rows: Vec<Value> | result: Vec<Value>
        let rows_owner: &mut Value = match obj.get_mut("result") {
            Some(r) => r,
            None => return 0,
        };
        let rows: &mut Vec<Value> = match rows_owner {
            Value::Object(map) => match map.get_mut("rows").and_then(|v| v.as_array_mut()) {
                Some(arr) => arr,
                None => return 0,
            },
            Value::Array(arr) => arr,
            _ => return 0,
        };
        if rows.is_empty() {
            return 0;
        }

        let mut guard = self.sessions.write().unwrap();
        // Защита от утечки: новая сессия при переполнении карты → полный сброс.
        if !guard.contains_key(sid) && guard.len() >= self.max_sessions {
            guard.clear();
        }
        let seen = guard.entry(sid.to_string()).or_default();
        let mut kept: Vec<Value> = Vec::with_capacity(rows.len());
        let mut elided = 0usize;
        for row in rows.drain(..) {
            let fp = fingerprint(&row);
            if seen.contains(&fp) {
                elided += 1;
            } else {
                if seen.len() < self.max_rows_per_session {
                    seen.insert(fp);
                }
                kept.push(row);
            }
        }
        *rows = kept;
        drop(guard);

        if elided > 0 {
            // Явный маркер рядом с rows (или на уровне result-массива — в объект).
            if let Value::Object(map) = rows_owner {
                map.insert(
                    "rows_elided_already_delivered".to_string(),
                    Value::from(elided),
                );
            } else if let Value::Array(_) = rows_owner {
                // result — голый массив: обернуть в объект нельзя без слома формы,
                // поэтому маркер не вставляем, но строки опущены (число вернём).
            }
        }
        elided
    }
}

/// Усечённый до u64 sha256 канонической (с сортировкой ключей) сериализации
/// строки таблицы. Коллизия на 50k строк ≈ 1e-10 — пренебрежимо для дедупа.
fn fingerprint(row: &Value) -> u64 {
    let canon = serde_json::to_string(&sort_keys(row.clone())).unwrap_or_default();
    let digest = Sha256::digest(canon.as_bytes());
    u64::from_le_bytes(digest[..8].try_into().unwrap())
}

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

/// Индекс первого элемента `content[]` с `type=="text"`.
fn find_text_content_index(outer: &Value) -> Option<usize> {
    outer
        .get("content")?
        .as_array()?
        .iter()
        .position(|item| item.get("type").and_then(|t| t.as_str()) == Some("text"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mcp_payload(rows: Value) -> String {
        // Имитация CallToolResult: content[0].text = вложенный {result:{rows}}
        let inner = json!({ "result": { "rows": rows } }).to_string();
        json!({ "content": [ { "type": "text", "text": inner } ] }).to_string()
    }
    fn rows_of(payload: &str) -> Vec<Value> {
        let outer: Value = serde_json::from_str(payload).unwrap();
        let text = outer["content"][0]["text"].as_str().unwrap();
        let inner: Value = serde_json::from_str(text).unwrap();
        inner["result"]["rows"].as_array().unwrap().clone()
    }

    #[test]
    fn first_delivery_keeps_all() {
        let d = SessionDedup::new(true);
        let p = mcp_payload(json!([["A", 1], ["B", 2]]));
        let (out, elided) = d.process(Some("s1"), &p);
        assert_eq!(elided, 0);
        assert_eq!(rows_of(&out).len(), 2);
    }

    #[test]
    fn second_delivery_elides_repeats() {
        let d = SessionDedup::new(true);
        let p = mcp_payload(json!([["A", 1], ["B", 2]]));
        d.process(Some("s1"), &p); // первая доставка
        let p2 = mcp_payload(json!([["A", 1], ["B", 2], ["C", 3]]));
        let (out, elided) = d.process(Some("s1"), &p2);
        assert_eq!(elided, 2); // A,B уже отданы
        let kept = rows_of(&out);
        assert_eq!(kept.len(), 1); // только C
        // маркер на месте
        let outer: Value = serde_json::from_str(&out).unwrap();
        let inner: Value = serde_json::from_str(outer["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(inner["result"]["rows_elided_already_delivered"], json!(2));
    }

    #[test]
    fn sessions_are_isolated() {
        let d = SessionDedup::new(true);
        let p = mcp_payload(json!([["A", 1]]));
        d.process(Some("s1"), &p);
        let (_, elided) = d.process(Some("s2"), &p); // другая сессия — не опускаем
        assert_eq!(elided, 0);
    }

    #[test]
    fn no_session_no_dedup() {
        let d = SessionDedup::new(true);
        let p = mcp_payload(json!([["A", 1]]));
        d.process(None, &p);
        let (_, elided) = d.process(None, &p);
        assert_eq!(elided, 0);
    }

    #[test]
    fn disabled_passthrough() {
        let d = SessionDedup::new(false);
        let p = mcp_payload(json!([["A", 1]]));
        d.process(Some("s1"), &p);
        let (out, elided) = d.process(Some("s1"), &p);
        assert_eq!(elided, 0);
        assert_eq!(rows_of(&out).len(), 1);
    }

    #[test]
    fn non_tabular_untouched() {
        let d = SessionDedup::new(true);
        // result — объект без rows (например, get_function отдаёт массив записей
        // под другим ключом) → не трогаем
        let inner = json!({ "result": { "functions": [{"name": "X"}] } }).to_string();
        let p = json!({ "content": [ { "type": "text", "text": inner } ] }).to_string();
        let (out, elided) = d.process(Some("s1"), &p);
        assert_eq!(elided, 0);
        assert_eq!(out, p);
    }
}
