// MCP-tool `get_register_writers` — регистраторы регистра и движения документа.
//
// Отвечает сразу на два встречных вопроса по recorder-рёбрам таблицы
// `data_links` (link_kind = "recorder", документ → регистр):
//   * «какие документы пишут движения в регистр R» (object = регистр) →
//     поле `writers`;
//   * «в какие регистры пишет документ D» (object = документ) →
//     поле `writes_to`.
//
// Источник рёбер — декларативный состав `<RegisterRecords>` в XML каждого
// документа (а не разбор кода проведения) — это точный список регистраторов
// из метаданных, без ложных срабатываний.
//
// Закрывает пробел, из-за которого `get_data_links(register, direction=in)`
// не показывал движения: тот граф моделирует ссылочные реквизиты, а
// «документ пишет в регистр» — отдельный вид связи. Здесь он целевой и
// не тонет среди ссылочных рёбер.
//
// Защита контекста: каждая сторона ограничена `CAP` именами (возвращаются
// только имена объектов, поэтому потолок высокий — страховка от
// патологически связанных объектов). При обрезке — `writers_truncated` /
// `writes_to_truncated`.

use std::future::Future;
use std::pin::Pin;

use code_index_core::extension::{IndexTool, ToolContext};
use rusqlite::params;
use serde_json::{json, Value};

/// Потолок имён на сторону (writers/writes_to). Возвращаются только имена
/// объектов — поэтому высокий; защита от патологических случаев.
const CAP: i64 = 500;

pub struct GetRegisterWritersTool;

impl IndexTool for GetRegisterWritersTool {
    fn name(&self) -> &str {
        "get_register_writers"
    }

    fn description(&self) -> &str {
        "Регистраторы регистра и движения документа 1С по recorder-рёбрам \
         (составу движений из метаданных). Для регистра (например \
         'AccumulationRegister.ТоварыНаСкладах') возвращает в 'writers' список \
         документов, пишущих в него движения. Для документа (например \
         'Document.РеализацияТоваровУслуг') возвращает в 'writes_to' список \
         регистров, в которые он пишет. Один вызов закрывает оба направления — \
         тип объекта определять заранее не нужно. Точнее разбора кода проведения \
         (источник — декларативный состав движений документа). Каждая сторона \
         ограничена 500 именами (при обрезке — writers_truncated/writes_to_truncated). \
         For BSL/1C repositories only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Алиас репозитория" },
                "object": {
                    "type": "string",
                    "description": "Канонический объект: регистр ('AccumulationRegister.ТоварыНаСкладах', 'InformationRegister.Цены', 'AccountingRegister.Хозрасчетный') или документ ('Document.РеализацияТоваровУслуг')"
                }
            },
            "required": ["repo", "object"]
        })
    }

    fn applicable_languages(&self) -> Option<&'static [&'static str]> {
        Some(&["bsl"])
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Value> + Send + 'a>> {
        Box::pin(async move {
            let object = match args.get("object").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => {
                    return crate::tools::wrap_error(json!({
                        "error": "missing required parameter 'object' (string)"
                    }));
                }
            };

            let storage = match ctx.storage.get().await {
                Ok(s) => s,
                Err(e) => {
                    return crate::tools::wrap_error(serde_json::json!({
                        "error": format!("storage pool: {}", e)
                    }));
                }
            };
            let conn = storage.conn();

            // writers — кто пишет в этот объект как в регистр (to_object = object).
            let (writers, writers_truncated) = match query_recorders(conn, &object, Side::Writers) {
                Ok(v) => v,
                Err(e) => {
                    return crate::tools::wrap_error(json!({
                        "error": format!("database error (writers): {}", e)
                    }))
                }
            };
            // writes_to — в какие регистры пишет этот объект как документ
            // (from_object = object).
            let (writes_to, writes_to_truncated) =
                match query_recorders(conn, &object, Side::WritesTo) {
                    Ok(v) => v,
                    Err(e) => {
                        return crate::tools::wrap_error(json!({
                            "error": format!("database error (writes_to): {}", e)
                        }))
                    }
                };

            crate::tools::wrap_with_meta(
                json!({
                    "object": object,
                    "writers": writers,
                    "writers_truncated": writers_truncated,
                    "writes_to": writes_to,
                    "writes_to_truncated": writes_to_truncated,
                }),
                Vec::new(),
            )
        })
    }
}

/// Сторона запроса по recorder-рёбрам.
enum Side {
    /// Документы, пишущие в `object` (object стоит как to_object — регистр).
    Writers,
    /// Регистры, в которые пишет `object` (object стоит как from_object — документ).
    WritesTo,
}

/// Выбрать встречную сторону recorder-рёбер для объекта, не более CAP штук.
/// Writers  → from_object WHERE to_object = object.
/// WritesTo → to_object   WHERE from_object = object.
/// Возвращает (имена, truncated).
fn query_recorders(
    conn: &rusqlite::Connection,
    object: &str,
    side: Side,
) -> rusqlite::Result<(Vec<String>, bool)> {
    let sql = match side {
        Side::Writers => {
            "SELECT DISTINCT from_object FROM data_links \
             WHERE repo = ?1 AND link_kind = 'recorder' AND to_object = ?2 \
             ORDER BY from_object LIMIT ?3"
        }
        Side::WritesTo => {
            "SELECT DISTINCT to_object FROM data_links \
             WHERE repo = ?1 AND link_kind = 'recorder' AND from_object = ?2 \
             ORDER BY to_object LIMIT ?3"
        }
    };
    let mut stmt = conn.prepare(sql)?;
    // CAP+1 — чтобы отличить «ровно CAP» от «есть ещё».
    let rows = stmt.query_map(params!["default", object, CAP + 1], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    let truncated = out.len() as i64 > CAP;
    if truncated {
        out.truncate(CAP as usize);
    }
    Ok((out, truncated))
}
