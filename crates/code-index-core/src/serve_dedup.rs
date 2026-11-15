//! Сессионный дедуп РЕ-ДОСТАВКИ результатов tool-вызовов.
//!
//! Идея: в рамках ОДНОЙ сессии не отдавать повторно строки результата, уже
//! доставленные ранее в этой же сессии (модель их уже видела в своём контексте).
//! Ключ — session id (из заголовка `mcp-session-id`, см. `call_tool`).
//!
//! Гранулярность — ЭЛЕМЕНТ (строка результата). Обрабатываются обе формы
//! списка: `result: {rows:[...]}` (таблица `bsl_sql`) и `result: [...]` (голый
//! массив — так отвечает большинство инструментов: `get_callers`,
//! `get_function`, `find_symbol`, `list_files` и др.). Прочие формы проходят
//! без изменений (консервативно — не трогаем то, что не умеем безопасно
//! переписать).
//!
//! Маркер вместо тишины — обязателен для ОБЕИХ форм: опущенные строки
//! сопровождаются полем `rows_elided_already_delivered: N` и подсказкой
//! [`HINT_ELIDED`]. Для таблицы маркер кладётся внутрь `result` рядом с `rows`;
//! для голого массива — рядом с самим `result`, в объемлющий объект (внутрь
//! массива служебное поле не вставить, не сломав форму; рядом с `result` уже
//! живут `hint` и `truncated/total/limit`, см. `tools::wrap_with_meta_extra`).
//!
//! Это correctness-sensitive (в отличие от прозрачного кэша): молча опущенный
//! результат неотличим от честного «данных нет», и модель по нему делает вывод
//! «код не используется» и правит живое. Ровно этот случай — issue #5 (0.47.0),
//! где по http повторный `get_callers` отдавал пустой список без признаков.
//!
//! ОБЛАСТЬ отпечатка — «инструмент|репо» (см. [`fingerprint`]). Строки разных
//! репо и разных инструментов в одно множество не смешиваются: без этого первый
//! же запрос к соседней базе приходил пустым, если строки текстуально совпали
//! (`ut` против `ut-test` — одинаковые имена объектов 1С). Аргументы вызова в
//! область НЕ входят намеренно: модель часто переспрашивает то же самое с
//! уточнённым `limit`/`path_glob`, и по аргументам отсев такие повторы бы
//! пропускал. Плата — разные вызовы одного инструмента могут гасить строки друг
//! друга; это безопасно ровно потому, что каждое опущение теперь помечено.
//!
//! Включение/выключение — `daemon.toml [mcp].dedup_enabled` (дефолт `true`);
//! полный сброс памяти — `POST /dedup-reset` (доступен любому клиенту: изнутри
//! MCP сжатие контекста модели не наблюдаемо ни у одного из них).

use crate::serve_cache::{lock_r, lock_w};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

/// Подсказка, сопровождающая опущенные строки. Намеренно по-английски: ответы
/// сервера читают любые модели и клиенты, а не только русскоязычная связка, и
/// слабые модели понимают английскую формулировку заметно надёжнее. От этой
/// фразы зависит вывод «мёртвый код или нет» — потому она прямая и без терминов.
const HINT_ELIDED: &str = "Some rows were omitted here because they were already \
delivered earlier in THIS session. An empty or shortened list does NOT mean the data \
is absent — do not conclude that the code is unused. Look for the earlier result \
above in your context; if it is no longer there, query again by other means.";

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

pub(crate) struct DedupDelivery<'a> {
    owner: &'a SessionDedup,
    sessions: Option<std::sync::RwLockWriteGuard<'a, HashMap<String, HashSet<u64>>>>,
    session_id: Option<String>,
    seen: HashSet<u64>,
    pending: HashMap<String, PendingScope>,
}

#[derive(Default)]
struct PendingScope {
    fingerprints: HashSet<u64>,
    elided: usize,
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
        lock_w(&self.sessions).remove(session_id);
    }

    /// Забыть состояние ВСЕХ сессий (ручка `POST /dedup-reset`). Возвращает
    /// число забытых сессий. Нужна там, где модель потеряла часть контекста
    /// (сжатие истории у клиента), а сессия MCP при этом не рвалась: изнутри
    /// протокола это событие не наблюдаемо, поэтому сброс инициируется снаружи.
    /// Сброс по всем сессиям сразу — `mcp-session-id` клиенту неизвестен;
    /// цена — разовая повторная отдача строк, корректность не страдает (тем же
    /// приёмом сбрасывается карта при переполнении).
    pub fn reset_all(&self) -> usize {
        let mut guard = lock_w(&self.sessions);
        let n = guard.len();
        guard.clear();
        n
    }

    /// (сессий в памяти, всего опущено строк) — для /cache-stats.
    pub fn stats(&self) -> (usize, u64) {
        (
            lock_r(&self.sessions).len(),
            self.elided_total.load(Ordering::Relaxed),
        )
    }

    /// Обработать сериализованный `CallToolResult` (JSON-строку): опустить строки
    /// табличного результата, уже отданные в этой сессии, и запомнить новые.
    /// Возвращает (возможно переписанный payload, число опущенных строк).
    /// Если форма не табличная / session_id нет / дедуп выключен — payload без
    /// изменений и 0.
    /// `scope` — область строки («инструмент|репо»): строки разных репо и разных
    /// инструментов в одно множество не смешиваются (см. модульный комментарий).
    pub fn process(&self, session_id: Option<&str>, scope: &str, payload: &str) -> (String, usize) {
        let mut delivery = self.begin_delivery(session_id);
        let result = delivery.process(scope, payload);
        delivery.commit([scope]);
        result
    }

    /// Подготовить доставку под общим lock; до commit изменения только локальные.
    pub(crate) fn begin_delivery(&self, session_id: Option<&str>) -> DedupDelivery<'_> {
        let session_id = session_id.filter(|_| self.enabled);
        let sessions = session_id.map(|_| lock_w(&self.sessions));
        let seen = session_id
            .and_then(|sid| sessions.as_ref()?.get(sid))
            .cloned()
            .unwrap_or_default();
        DedupDelivery {
            owner: self,
            sessions,
            session_id: session_id.map(str::to_string),
            seen,
            pending: HashMap::new(),
        }
    }

    /// Повторный проход по ДУБЛИРУЮЩЕМУ представлению того же ответа
    /// (`structuredContent`): опустить ровно те строки, что опущены первым
    /// проходом. Память сессии не трогает — иначе опустил бы весь ответ, ведь
    /// новые строки первый проход уже успел запомнить.
    fn apply_elision(&self, elided_fps: &HashSet<u64>, scope: &str, obj: &mut Value) -> usize {
        if elided_fps.is_empty() {
            return 0;
        }
        Self::rewrite_rows(obj, &mut |row| {
            elided_fps.contains(&fingerprint(scope, row))
        })
    }

    /// Найти `result.rows` (или `result` как массив) в объекте `{result, _meta}`,
    /// опустить строки по решению `drop_row`, проставить маркер. Возвращает
    /// число опущенных. Память сессии сама не трогает — решение целиком за
    /// вызывающим.
    fn rewrite_rows(obj: &mut Value, drop_row: &mut dyn FnMut(&Value) -> bool) -> usize {
        // Форма результата решает, КУДА ляжет маркер: в таблице — внутрь
        // `result` рядом с `rows`, у голого массива — рядом с самим `result`
        // (запоминаем до мутабельного заимствования ниже).
        let bare_array = matches!(obj.get("result"), Some(Value::Array(_)));

        let elided = {
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

            let mut kept: Vec<Value> = Vec::with_capacity(rows.len());
            let mut elided = 0usize;
            for row in rows.drain(..) {
                if drop_row(&row) {
                    elided += 1;
                } else {
                    kept.push(row);
                }
            }
            *rows = kept;

            // Табличная форма: маркер внутрь `result`, рядом с `rows`.
            if elided > 0 {
                if let Value::Object(map) = rows_owner {
                    map.insert(
                        "rows_elided_already_delivered".to_string(),
                        Value::from(elided),
                    );
                    map.entry("hint".to_string())
                        .or_insert_with(|| Value::from(HINT_ELIDED));
                }
            }
            elided
        };

        // Голый массив: внутрь массива служебное поле не вставить, не сломав
        // форму, — кладём рядом с `result`, в объемлющий объект. Там уже живут
        // `hint` и `truncated/total/limit` (`tools::wrap_with_meta_extra`), так
        // что форма ответа не меняется. Чужой `hint` не затираем: при обрезке
        // по лимиту он занят и несёт другой смысл.
        if elided > 0 && bare_array {
            if let Some(map) = obj.as_object_mut() {
                map.insert(
                    "rows_elided_already_delivered".to_string(),
                    Value::from(elided),
                );
                map.entry("hint".to_string())
                    .or_insert_with(|| Value::from(HINT_ELIDED));
            }
        }
        elided
    }
}

impl DedupDelivery<'_> {
    pub(crate) fn process(&mut self, scope: &str, payload: &str) -> (String, usize) {
        if self.sessions.is_none() {
            return (payload.to_string(), 0);
        }
        let Ok(mut outer) = serde_json::from_str::<Value>(payload) else {
            return (payload.to_string(), 0);
        };

        // Данные tool'а лежат в MCP CallToolResult: content[*].text — это
        // вложенная JSON-строка `{result, _meta}`. Находим её, дедупим, кладём
        // обратно. structuredContent (если есть) дублирует ТЕ ЖЕ данные.
        let mut elided = 0usize;
        // Отпечатки строк, опущенных первым проходом. Второй проход обязан
        // опустить РОВНО их: память сессии он не трогает и своего решения не
        // принимает. Иначе он увидит в памяти строки, только что запомненные
        // первым проходом, и опустит весь ответ до единой строки.
        let mut elided_fps: HashSet<u64> = HashSet::new();
        let mut text_pass_done = false;

        // 1) content[0].text (вложенный JSON-string)
        if let Some(text_idx) = find_text_content_index(&outer) {
            if let Some(text) = outer["content"][text_idx]["text"].as_str() {
                if let Ok(mut inner) = serde_json::from_str::<Value>(text) {
                    let (n, fps) = self.dedup_and_record(scope, &mut inner);
                    elided += n;
                    elided_fps = fps;
                    text_pass_done = true;
                    if n > 0 {
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
            if text_pass_done {
                // Дубль уже обработанного ответа — повторяем решение первого прохода.
                self.owner.apply_elision(&elided_fps, scope, &mut sc);
            } else {
                // Текстовой части не было: структурная форма — единственный
                // источник, обрабатываем её полноценно.
                elided += self.dedup_and_record(scope, &mut sc).0;
            }
            outer["structuredContent"] = sc;
        }

        if elided == 0 {
            return (payload.to_string(), 0);
        }
        match serde_json::to_string(&outer) {
            Ok(s) => {
                self.pending.entry(scope.to_string()).or_default().elided += elided;
                (s, elided)
            }
            Err(_) => (payload.to_string(), 0),
        }
    }

    fn dedup_and_record(&mut self, scope: &str, obj: &mut Value) -> (usize, HashSet<u64>) {
        let mut elided_fps = HashSet::new();
        let pending = self.pending.entry(scope.to_string()).or_default();
        let seen = &mut self.seen;
        let max_rows = self.owner.max_rows_per_session;
        let elided = SessionDedup::rewrite_rows(obj, &mut |row| {
            let fp = fingerprint(scope, row);
            if seen.contains(&fp) {
                elided_fps.insert(fp);
                true
            } else {
                if seen.len() < max_rows {
                    seen.insert(fp);
                    pending.fingerprints.insert(fp);
                }
                false
            }
        });
        (elided, elided_fps)
    }

    /// Фиксируются только группы, payload которых действительно дошёл до клиента.
    pub(crate) fn commit<'s>(mut self, scopes: impl IntoIterator<Item = &'s str>) {
        let (Some(sessions), Some(sid)) = (self.sessions.as_mut(), self.session_id.as_deref())
        else {
            return;
        };
        let mut committed = false;
        for scope in scopes {
            let Some(pending) = self.pending.remove(scope) else {
                continue;
            };
            if !committed {
                if !sessions.contains_key(sid) && sessions.len() >= self.owner.max_sessions {
                    sessions.clear();
                }
                committed = true;
            }
            let seen = sessions.entry(sid.to_string()).or_default();
            for fp in pending.fingerprints {
                if seen.len() >= self.owner.max_rows_per_session {
                    break;
                }
                seen.insert(fp);
            }
            self.owner
                .elided_total
                .fetch_add(pending.elided as u64, Ordering::Relaxed);
        }
    }
}

/// Усечённый до u64 sha256 от ОБЛАСТИ и канонической (с сортировкой ключей)
/// сериализации строки таблицы. Коллизия на 50k строк ≈ 1e-10 — пренебрежимо.
///
/// Область («инструмент|репо») входит в отпечаток обязательно: без неё строка
/// «AccumulationRegister.ent_Инвентарь», отданная из одного репо, гасила такую
/// же строку из другого — первый же запрос к соседней базе приходил пустым
/// (наблюдалось на `ut` против `ut-test`). Бьёт как раз по сравнению двух
/// конфигураций, где совпадения и есть предмет интереса.
fn fingerprint(scope: &str, row: &Value) -> u64 {
    let canon = serde_json::to_string(&sort_keys(row.clone())).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(scope.as_bytes());
    hasher.update([0u8]); // разделитель: область не должна «слипаться» с телом
    hasher.update(canon.as_bytes());
    let digest = hasher.finalize();
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

    /// Область строки в тестах — «инструмент|репо» (см. [`fingerprint`]).
    const SCOPE: &str = "get_callers|ut";

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

    fn dual_payload(result: Value) -> String {
        let inner = json!({ "result": result });
        json!({
            "content": [{"type": "text", "text": inner.to_string()}],
            "structuredContent": inner
        })
        .to_string()
    }

    fn synchronized_inner(payload: &str) -> Value {
        let outer: Value = serde_json::from_str(payload).unwrap();
        let inner: Value =
            serde_json::from_str(outer["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(inner, outer["structuredContent"]);
        inner
    }

    #[test]
    fn delivery_abort_and_commit_preserve_only_retained_scopes() {
        for table in [false, true] {
            let d = SessionDedup::new(true);
            let make = |rows: Value| dual_payload(if table { json!({"rows": rows}) } else { rows });
            let old = make(json!(["old"]));
            let mixed = make(json!(["old", "new"]));
            d.process(Some("s1"), "tool|alpha", &old);
            d.process(Some("s1"), "tool|beta", &old);
            {
                let mut delivery = d.begin_delivery(Some("s1"));
                assert_eq!(delivery.process("tool|alpha", &mixed).1, 1);
                assert_eq!(delivery.process("tool|beta", &mixed).1, 1);
                assert_eq!(delivery.sessions.as_ref().unwrap()["s1"].len(), 2);
                assert_eq!(d.elided_total.load(Ordering::Relaxed), 0);
            }
            assert_eq!(d.stats(), (1, 0));
            let mut delivery = d.begin_delivery(Some("s1"));
            delivery.process("tool|alpha", &mixed);
            delivery.process("tool|beta", &mixed);
            delivery.commit(["tool|alpha"]);
            assert_eq!(d.stats(), (1, 1));

            let (alpha, elided) = d.process(Some("s1"), "tool|alpha", &mixed);
            assert_eq!(elided, 2);
            let alpha = synchronized_inner(&alpha);
            let alpha = if table { &alpha["result"] } else { &alpha };
            assert_eq!(alpha["rows_elided_already_delivered"], 2);
            let (beta, elided) = d.process(Some("s1"), "tool|beta", &mixed);
            assert_eq!(elided, 1);
            let beta = synchronized_inner(&beta);
            if table {
                assert_eq!(beta["result"]["rows"], json!(["new"]));
                assert_eq!(beta["result"]["rows_elided_already_delivered"], 1);
            } else {
                assert_eq!(beta["result"], json!(["new"]));
                assert_eq!(beta["rows_elided_already_delivered"], 1);
            }
        }
    }

    #[test]
    fn delivery_without_session_or_enabled_dedup_is_inert() {
        for (enabled, session) in [(true, None), (false, Some("s1"))] {
            let d = SessionDedup::new(enabled);
            let payload = dual_payload(json!(["new"]));
            let mut delivery = d.begin_delivery(session);
            assert!(delivery.sessions.is_none());
            assert_eq!(delivery.process(SCOPE, &payload), (payload.clone(), 0));
            assert_eq!(delivery.process(SCOPE, &payload), (payload, 0));
            assert!(delivery.pending.is_empty());
            delivery.commit([SCOPE]);
            assert_eq!(d.stats(), (0, 0));
        }
    }

    #[test]
    fn delivery_memory_limits_apply_only_on_commit() {
        let mut d = SessionDedup::new(true);
        d.max_rows_per_session = 2;
        d.max_sessions = 1;
        let payload = mcp_payload(json!(["A", "B", "C"]));
        d.process(Some("old"), SCOPE, &payload);
        assert_eq!(lock_r(&d.sessions)["old"].len(), 2);
        for commit_empty in [false, true] {
            let mut delivery = d.begin_delivery(Some("new"));
            delivery.process(SCOPE, &payload);
            assert_eq!(delivery.sessions.as_ref().unwrap().len(), 1);
            assert!(delivery.sessions.as_ref().unwrap().contains_key("old"));
            if commit_empty {
                delivery.commit([]);
            } else {
                drop(delivery);
            }
            assert_eq!(d.stats(), (1, 0));
            assert!(lock_r(&d.sessions).contains_key("old"));
        }
        let mut delivery = d.begin_delivery(Some("new"));
        delivery.process(SCOPE, &payload);
        delivery.commit([SCOPE]);
        assert!(!lock_r(&d.sessions).contains_key("old"));
        assert_eq!(lock_r(&d.sessions)["new"].len(), 2);
        let (out, elided) = d.process(Some("new"), SCOPE, &payload);
        assert_eq!(elided, 2);
        assert_eq!(rows_of(&out), vec![json!("C")]);
        assert_eq!(lock_r(&d.sessions)["new"].len(), 2);
    }

    #[test]
    fn delivery_serializes_concurrent_writers_without_losing_rows() {
        use std::sync::{mpsc, Arc};

        for commit in [false, true] {
            let d = Arc::new(SessionDedup::new(true));
            let alpha = mcp_payload(json!(["A"]));
            let beta = mcp_payload(json!(["B"]));
            let mut delivery = d.begin_delivery(Some("s1"));
            delivery.process("tool|alpha", &alpha);
            delivery.process("tool|beta", &beta);
            let (ready_tx, ready_rx) = mpsc::channel();
            let (done_tx, done_rx) = mpsc::channel();
            let other = Arc::clone(&d);
            let worker = std::thread::spawn(move || {
                assert!(other.sessions.try_write().is_err());
                ready_tx.send(()).unwrap();
                let mut delivery = other.begin_delivery(Some("s1"));
                let alpha = delivery.process("tool|alpha", &mcp_payload(json!(["A", "C"])));
                let beta = delivery.process("tool|beta", &mcp_payload(json!(["B"])));
                delivery.commit(["tool|alpha", "tool|beta"]);
                done_tx.send((alpha, beta)).unwrap();
            });
            ready_rx.recv().unwrap();
            assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
            if commit {
                delivery.commit(["tool|alpha"]);
            } else {
                drop(delivery);
            }
            let ((alpha, elided), (beta, beta_elided)) = done_rx.recv().unwrap();
            worker.join().unwrap();
            assert_eq!(elided, usize::from(commit));
            assert_eq!(
                rows_of(&alpha),
                if commit {
                    vec![json!("C")]
                } else {
                    vec![json!("A"), json!("C")]
                }
            );
            assert_eq!(beta_elided, 0);
            assert_eq!(rows_of(&beta), vec![json!("B")]);
            assert_eq!(
                d.process(Some("s1"), "tool|alpha", &alpha).1,
                if commit { 1 } else { 2 }
            );
            assert_eq!(
                d.process(Some("s1"), "tool|alpha", &mcp_payload(json!(["A", "C"])))
                    .1,
                2
            );
            assert_eq!(d.process(Some("s1"), "tool|beta", &beta).1, 1);
        }
    }

    #[test]
    fn first_delivery_keeps_all() {
        let d = SessionDedup::new(true);
        let p = mcp_payload(json!([["A", 1], ["B", 2]]));
        let (out, elided) = d.process(Some("s1"), SCOPE, &p);
        assert_eq!(elided, 0);
        assert_eq!(rows_of(&out).len(), 2);
    }

    #[test]
    fn second_delivery_elides_repeats() {
        let d = SessionDedup::new(true);
        let p = mcp_payload(json!([["A", 1], ["B", 2]]));
        d.process(Some("s1"), SCOPE, &p); // первая доставка
        let p2 = mcp_payload(json!([["A", 1], ["B", 2], ["C", 3]]));
        let (out, elided) = d.process(Some("s1"), SCOPE, &p2);
        assert_eq!(elided, 2); // A,B уже отданы
        let kept = rows_of(&out);
        assert_eq!(kept.len(), 1); // только C
                                   // маркер на месте
        let outer: Value = serde_json::from_str(&out).unwrap();
        let inner: Value =
            serde_json::from_str(outer["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(inner["result"]["rows_elided_already_delivered"], json!(2));
    }

    #[test]
    fn sessions_are_isolated() {
        let d = SessionDedup::new(true);
        let p = mcp_payload(json!([["A", 1]]));
        d.process(Some("s1"), SCOPE, &p);
        let (_, elided) = d.process(Some("s2"), SCOPE, &p); // другая сессия — не опускаем
        assert_eq!(elided, 0);
    }

    /// Области не смешиваются: одинаковая строка из другого репо (и от другого
    /// инструмента) не считается уже отданной. Без этого первый же запрос к
    /// соседней базе приходил пустым — имена объектов 1С в `ut` и `ut-test`
    /// совпадают дословно.
    #[test]
    fn scopes_are_isolated() {
        let d = SessionDedup::new(true);
        let p = mcp_payload(json!([["AccumulationRegister.ent_Инвентарь"]]));
        d.process(Some("s1"), "bsl_sql|ut-test", &p);
        let (_, other_repo) = d.process(Some("s1"), "bsl_sql|ut", &p);
        assert_eq!(other_repo, 0);
        let (_, other_tool) = d.process(Some("s1"), "search_text|ut-test", &p);
        assert_eq!(other_tool, 0);
        // Та же область — отсев работает как прежде.
        let (_, same_scope) = d.process(Some("s1"), "bsl_sql|ut", &p);
        assert_eq!(same_scope, 1);
    }

    #[test]
    fn no_session_no_dedup() {
        let d = SessionDedup::new(true);
        let p = mcp_payload(json!([["A", 1]]));
        d.process(None, SCOPE, &p);
        let (_, elided) = d.process(None, SCOPE, &p);
        assert_eq!(elided, 0);
    }

    #[test]
    fn disabled_passthrough() {
        let d = SessionDedup::new(false);
        let p = mcp_payload(json!([["A", 1]]));
        d.process(Some("s1"), SCOPE, &p);
        let (out, elided) = d.process(Some("s1"), SCOPE, &p);
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
        let (out, elided) = d.process(Some("s1"), SCOPE, &p);
        assert_eq!(elided, 0);
        assert_eq!(out, p);
    }

    /// Голый массив `result: [...]` — форма ответа большинства инструментов
    /// (get_callers/get_function/find_symbol/list_files). Маркер обязан
    /// появиться РЯДОМ с `result`: без него повтор молча пуст и неотличим от
    /// «данных нет» — issue #5 (0.47.0).
    #[test]
    fn bare_array_marks_elided_next_to_result() {
        let d = SessionDedup::new(true);
        let inner = json!({ "result": [{"caller": "A"}, {"caller": "B"}] }).to_string();
        let p = json!({ "content": [ { "type": "text", "text": inner } ] }).to_string();
        d.process(Some("s1"), SCOPE, &p); // первая доставка — отданы обе строки
        let (out, elided) = d.process(Some("s1"), SCOPE, &p);
        assert_eq!(elided, 2);
        let outer: Value = serde_json::from_str(&out).unwrap();
        let inner: Value =
            serde_json::from_str(outer["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(inner["result"].as_array().unwrap().len(), 0);
        assert_eq!(inner["rows_elided_already_delivered"], json!(2));
        assert!(inner["hint"].as_str().unwrap().contains("does NOT mean"));
    }

    /// Изначально пустой список — это НЕ результат отсева: пометки быть не
    /// должно, иначе «данных нет» не отличить от «всё уже отдавали».
    #[test]
    fn originally_empty_result_gets_no_marker() {
        let d = SessionDedup::new(true);
        let inner = json!({ "result": [], "hint": "0 вызывателей" }).to_string();
        let p = json!({ "content": [ { "type": "text", "text": inner } ] }).to_string();
        let (out, elided) = d.process(Some("s1"), SCOPE, &p);
        assert_eq!(elided, 0);
        assert_eq!(out, p);
    }

    /// Чужой `hint` (обрезка по лимиту) не затирается — он несёт другой смысл;
    /// число опущенных строк при этом всё равно проставляется.
    #[test]
    fn foreign_hint_is_kept() {
        let d = SessionDedup::new(true);
        let inner = json!({ "result": [{"caller": "A"}], "hint": "truncated", "truncated": true })
            .to_string();
        let p = json!({ "content": [ { "type": "text", "text": inner } ] }).to_string();
        d.process(Some("s1"), SCOPE, &p);
        let (out, elided) = d.process(Some("s1"), SCOPE, &p);
        assert_eq!(elided, 1);
        let outer: Value = serde_json::from_str(&out).unwrap();
        let inner: Value =
            serde_json::from_str(outer["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(inner["hint"], json!("truncated"));
        assert_eq!(inner["rows_elided_already_delivered"], json!(1));
    }

    /// Регресс M-1: `structuredContent` дублирует те же данные, что и текстовая
    /// часть ответа. Раньше второй проход шёл по уже пополненному множеству
    /// отпечатков и опускал ВЕСЬ ответ — клиент, читающий структурную форму,
    /// терял именно новые строки, да ещё с пометкой «уже отдавали ранее».
    #[test]
    fn структурная_форма_не_опустошается_вторым_проходом() {
        let d = SessionDedup::new(true);
        let mk = |rows: Value| {
            let inner = json!({ "result": { "rows": rows } });
            json!({
                "content": [ { "type": "text", "text": inner.to_string() } ],
                "structuredContent": inner
            })
            .to_string()
        };
        d.process(Some("s1"), SCOPE, &mk(json!([["A", 1], ["B", 2]])));
        let (out, elided) = d.process(
            Some("s1"),
            SCOPE,
            &mk(json!([["A", 1], ["B", 2], ["C", 3]])),
        );
        assert_eq!(elided, 2);

        let outer: Value = serde_json::from_str(&out).unwrap();
        let text: Value =
            serde_json::from_str(outer["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text["result"]["rows"], json!([["C", 3]]));
        assert_eq!(
            outer["structuredContent"]["result"]["rows"],
            json!([["C", 3]]),
            "структурная форма обязана содержать те же строки, что и текстовая"
        );
        assert_eq!(
            outer["structuredContent"]["result"]["rows_elided_already_delivered"],
            json!(2)
        );
    }

    /// Ответ только со структурной формой (текстовой части нет) обрабатывается
    /// полноценно, а не как повторение несостоявшегося первого прохода.
    #[test]
    fn структурная_форма_без_текста_обрабатывается_самостоятельно() {
        let d = SessionDedup::new(true);
        let p = json!({ "structuredContent": { "result": [{"caller": "A"}] } }).to_string();
        let (_, first) = d.process(Some("s1"), SCOPE, &p);
        assert_eq!(first, 0, "первая доставка ничего не опускает");
        let (out, second) = d.process(Some("s1"), SCOPE, &p);
        assert_eq!(second, 1, "повтор обязан быть опущен");
        let outer: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            outer["structuredContent"]["result"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    /// `POST /dedup-reset` → память забыта, строки приходят снова (клиент сжал
    /// контекст, а сессия MCP при этом не рвалась).
    #[test]
    fn reset_all_returns_rows_again() {
        let d = SessionDedup::new(true);
        let inner = json!({ "result": [{"caller": "A"}] }).to_string();
        let p = json!({ "content": [ { "type": "text", "text": inner } ] }).to_string();
        d.process(Some("s1"), SCOPE, &p);
        assert_eq!(d.reset_all(), 1);
        let (_, elided) = d.process(Some("s1"), SCOPE, &p);
        assert_eq!(elided, 0);
    }
}
