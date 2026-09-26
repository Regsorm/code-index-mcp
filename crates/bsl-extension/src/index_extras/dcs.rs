//! Разбор макетов «Схема компоновки данных» (СКД) в таблицы `dcs_schemas` /
//! `dcs_datasets` и рёбра `data_links` вида `dcs_query`.
//!
//! Отдельный модуль, а не рост `full_scan.rs`: им пользуются и полный проход
//! (`index_dcs_schemas`), и наблюдатель (точечные `rebuild_dcs_schema` /
//! `delete_dcs_for_*`) — общий разбор путей и записи держим в одном месте.
//!
//! Где лежит содержимое схемы:
//!   * макет объекта, формат Конфигуратора —
//!     `<Вид>/<Объект>/Templates/<Имя>/Ext/Template.<ext>`;
//!   * макет объекта, выгрузка 1C:EDT —
//!     `<src>/<Вид>/<Объект>/Templates/<Имя>/Template.<ext>`;
//!   * общий макет — `CommonTemplates/<Имя>/Ext/Template.<ext>` (Конфигуратор)
//!     либо `<src>/CommonTemplates/<Имя>/Template.<ext>` (EDT).
//!
//! Расширение файла содержимого зависит от вида макета и заранее не известно
//! (у схемы компоновки это `.xml` в Конфигураторе и `.dcs` в EDT), поэтому
//! файл ищется по стему `Template` и опознаётся по корню
//! `xml::dcs::is_dcs_content`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::params;

use crate::code_usages::extract_query_usages;
use crate::xml::dcs::{self, DcsDataSet, DcsSchema};

use super::common::{like_escape, rel_path, sub_config_roots, REPO_DEFAULT};
use super::full_scan::template_content_in_dir;

/// Макет-схема компоновки, подлежащий разбору.
pub(crate) struct DcsTarget {
    /// Полное имя макета как строка перечня: `<Владелец>.Template.<Имя>` для
    /// макета объекта и `CommonTemplate.<Имя>` для общего макета.
    pub(crate) template_full_name: String,
    /// Владелец макета. У общего макета это ОН САМ (`CommonTemplate.<Имя>`) —
    /// см. комментарий таблицы `dcs_schemas`.
    pub(crate) owner_full_name: String,
    /// Абсолютный путь файла содержимого.
    pub(crate) content_abs: PathBuf,
    /// Путь файла содержимого относительно корня репо (`files.path`).
    pub(crate) content_rel: String,
}

/// Разделить полное имя макета на `(владелец, имя макета)`:
///   * `Report.X.Template.Y` → `("Report.X", "Y")`;
///   * `CommonTemplate.Y` → `("CommonTemplate.Y", "Y")` — у общего макета
///     своего объекта-владельца в выгрузке нет, поэтому владельцем записан он
///     сам (то же значение идёт в `from_object` его рёбер `dcs_query`).
pub(crate) fn split_template_full_name(full: &str) -> (String, String) {
    if let Some(rest) = full.strip_prefix("CommonTemplate.") {
        return (full.to_string(), rest.to_string());
    }
    if let Some(idx) = full.find(".Template.") {
        let owner = &full[..idx];
        let name = &full[idx + ".Template.".len()..];
        return (owner.to_string(), name.to_string());
    }
    match full.rfind('.') {
        Some(i) => (full[..i].to_string(), full[i + 1..].to_string()),
        None => (full.to_string(), full.to_string()),
    }
}

/// Собрать все макеты-схемы компоновки репо: где лежит содержимое каждого.
pub(crate) fn dcs_targets(
    repo_root: &Path,
    edt_src: Option<&Path>,
    conn: &rusqlite::Connection,
) -> Vec<DcsTarget> {
    let mut out: Vec<DcsTarget> = Vec::new();

    // (а) макеты объектов: у строки перечня есть паспорт с видом макета и
    // путём содержимого (путь относительный — склеиваем с корнем репо).
    // Выборка забирается целиком в своей области видимости: `MappedRows`
    // держит заимствование `Statement`, и жить дольше него не может.
    let mut passports: Vec<(String, Option<String>)> = Vec::new();
    {
        let mut stmt = match conn.prepare(
            "SELECT full_name, attributes_json FROM metadata_objects \
             WHERE repo = ?1 AND meta_type = 'Template'",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("dcs targets: {}", e);
                return out;
            }
        };
        match stmt.query_map(params![REPO_DEFAULT], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        }) {
            Ok(rows) => passports.extend(rows.flatten()),
            Err(e) => {
                tracing::warn!("dcs targets: {}", e);
                return out;
            }
        };
        // Точка с запятой выше не лишняя: хвостовое выражение блока продлило бы
        // жизнь временного `MappedRows` за пределы блока — дольше `stmt`.
    }
    for (full_name, json) in passports {
        let Some(json) = json else { continue };
        let Ok(p) = serde_json::from_str::<serde_json::Value>(&json) else {
            continue;
        };
        if p.get("template_type").and_then(|v| v.as_str()) != Some("DataCompositionSchema") {
            continue;
        }
        let Some(rel) = p.get("content_file").and_then(|v| v.as_str()) else {
            continue;
        };
        let (owner, _name) = split_template_full_name(&full_name);
        out.push(DcsTarget {
            template_full_name: full_name,
            owner_full_name: owner,
            content_abs: repo_root.join(rel),
            content_rel: rel.to_string(),
        });
    }

    // (б) общие макеты: паспорта с видом макета у них нет (путь
    // `CommonTemplates/<Имя>` не привязан к объекту), поэтому содержимое ищем
    // по раскладке выгрузки и опознаём по корню файла. Вид макета из пути не
    // виден — ровно для этого и нужен `is_dcs_content`.
    let mut common: Vec<(String, String)> = Vec::new();
    {
        let mut stmt = match conn.prepare(
            "SELECT full_name, name FROM metadata_objects \
             WHERE repo = ?1 AND meta_type = 'CommonTemplate'",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("dcs targets: {}", e);
                return out;
            }
        };
        if let Ok(rows) = stmt.query_map(params![REPO_DEFAULT], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            for row in rows.flatten() {
                common.push(row);
            }
        };
    }

    if !common.is_empty() {
        let roots = sub_config_roots(repo_root);
        for (full_name, name) in common {
            let mut candidates: Vec<PathBuf> = Vec::new();
            if let Some(src) = edt_src {
                candidates.push(src.join("CommonTemplates").join(&name));
            }
            for root in &roots {
                candidates.push(root.join("CommonTemplates").join(&name).join("Ext"));
            }
            let Some(abs) = candidates
                .iter()
                .map(|dir| dir.as_path())
                .find_map(template_content_in_dir)
            else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&abs) else {
                continue;
            };
            if !dcs::is_dcs_content(&text) {
                continue;
            }
            let rel = rel_path(repo_root, &abs);
            out.push(DcsTarget {
                template_full_name: full_name.clone(),
                owner_full_name: full_name,
                content_abs: abs,
                content_rel: rel,
            });
        }
    }

    out
}

/// Пересобрать строки одной схемы компоновки: снести прежние строки и рёбра
/// этой схемы, разобрать файл заново и записать. Возвращает
/// `(число наборов, число рёбер)`. Транзакцию НЕ ведёт — её ведёт вызывающий
/// (как `insert_template_row`).
///
/// Файл больше 16 МБ, нечитаемый или неразборный → предупреждение и `(0, 0)`:
/// ошибка одной схемы не должна ронять весь проход.
pub(crate) fn rebuild_dcs_schema(
    conn: &rusqlite::Connection,
    target: &DcsTarget,
) -> Result<(usize, usize)> {
    /// Предел размера файла содержимого: схемы типовых отчётов — десятки
    /// килобайт, 16 МБ отсекает заведомо не-схемы (случайный большой макет).
    const MAX_DCS_BYTES: u64 = 16 * 1024 * 1024;

    let meta = match std::fs::metadata(&target.content_abs) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                "dcs {}: {} ({})",
                target.template_full_name,
                e,
                target.content_abs.display()
            );
            return Ok((0, 0));
        }
    };
    if meta.len() > MAX_DCS_BYTES {
        tracing::warn!(
            "dcs {}: файл {} байт превышает предел {} — пропущен",
            target.template_full_name,
            meta.len(),
            MAX_DCS_BYTES
        );
        return Ok((0, 0));
    }
    let content = match std::fs::read_to_string(&target.content_abs) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                "dcs {}: {} ({})",
                target.template_full_name,
                e,
                target.content_abs.display()
            );
            return Ok((0, 0));
        }
    };
    let schema = match dcs::parse_dcs(&content) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("dcs {}: {}", target.template_full_name, e);
            return Ok((0, 0));
        }
    };

    delete_dcs_for_template(conn, &target.template_full_name)?;

    // Наборы — плоским списком; вложенные (части объединения) ссылаются на
    // родителя через `parent_set`.
    let mut flat: Vec<(&DcsDataSet, String)> = Vec::new();
    flatten_sets(&schema.data_sets, "", &mut flat);

    let mut fields_total = 0usize;
    {
        let mut ins = conn.prepare(
            "INSERT INTO dcs_datasets \
             (repo, template_full_name, data_set_name, kind, data_source, object_name, \
              query_text, fields_json, parent_set) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for (ds, parent) in &flat {
            fields_total += ds.fields.len();
            let fields_json = serde_json::to_string(&ds.fields)?;
            ins.execute(params![
                REPO_DEFAULT,
                &target.template_full_name,
                &ds.name,
                &ds.kind,
                &ds.data_source,
                &ds.object_name,
                &ds.query,
                &fields_json,
                parent,
            ])?;
        }
    }

    let (_, template_name) = split_template_full_name(&target.template_full_name);
    let edges = dcs_query_edges(&schema, &template_name);

    conn.execute(
        "INSERT INTO dcs_schemas \
         (repo, template_full_name, owner_full_name, title, content_file, data_sets_json, \
          links_json, calculated_fields_json, totals_json, parameters_json, variants_json, \
          templates_count, data_sets_count, fields_count, links_count, calculated_fields_count, \
          totals_count, parameters_count, variants_count) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            REPO_DEFAULT,
            &target.template_full_name,
            &target.owner_full_name,
            &schema.title,
            &target.content_rel,
            &serde_json::to_string(&strip_queries(&schema.data_sets))?,
            &serde_json::to_string(&schema.links)?,
            &serde_json::to_string(&schema.calculated_fields)?,
            &serde_json::to_string(&schema.totals)?,
            &serde_json::to_string(&schema.parameters)?,
            &serde_json::to_string(&schema.variants)?,
            schema.templates_count as i64,
            flat.len() as i64,
            fields_total as i64,
            schema.links.len() as i64,
            schema.calculated_fields.len() as i64,
            schema.totals.len() as i64,
            schema.parameters.len() as i64,
            schema.variants.len() as i64,
        ],
    )?;

    // Рёбра «макет читает объект»: `to_object_key` заполняем в ТОМ ЖЕ INSERT
    // (как в остальных точках записи `data_links`) — отдельный backfill не
    // нужен, а инструменты реверс-поиска ждут заполненный ключ.
    let mut edge_count = 0usize;
    {
        let mut ins = conn.prepare(
            "INSERT OR IGNORE INTO data_links \
             (repo, from_object, from_path, to_object, link_kind, is_composite, is_universal, \
              to_object_key) \
             VALUES (?1, ?2, ?3, ?4, 'dcs_query', 0, 0, ?5)",
        )?;
        for (from_path, to_object) in &edges {
            ins.execute(params![
                REPO_DEFAULT,
                &target.owner_full_name,
                from_path,
                to_object,
                &to_object.to_lowercase(),
            ])?;
            edge_count += 1;
        }
    }

    Ok((flat.len(), edge_count))
}

/// Плоский список наборов: набор + имя родителя (пустая строка у верхнего).
fn flatten_sets<'a>(sets: &'a [DcsDataSet], parent: &str, out: &mut Vec<(&'a DcsDataSet, String)>) {
    for ds in sets {
        out.push((ds, parent.to_string()));
        flatten_sets(&ds.items, &ds.name, out);
    }
}

/// Копия наборов БЕЗ текстов запросов: в `dcs_schemas.data_sets_json` тексты
/// не дублируются (они живут только в `dcs_datasets.query_text`).
fn strip_queries(sets: &[DcsDataSet]) -> Vec<DcsDataSet> {
    sets.iter()
        .map(|ds| {
            let mut c = ds.clone();
            c.query = None;
            c.items = strip_queries(&ds.items);
            c
        })
        .collect()
}

/// Рёбра «макет читает объект»: рекурсивно по наборам (включая `items`
/// объединений) — `extract_query_usages` по тексту запроса, `from_path` =
/// `«<ИмяМакета>.<ИмяНабора>»`, `to_object` = найденный объект.
/// `BTreeSet` даёт схлопывание дублей (один объект в разных строках запроса —
/// одно ребро).
fn dcs_query_edges(schema: &DcsSchema, template_name: &str) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    collect_edges(&schema.data_sets, template_name, &mut out);
    out
}

fn collect_edges(sets: &[DcsDataSet], template_name: &str, out: &mut BTreeSet<(String, String)>) {
    for ds in sets {
        if let Some(q) = &ds.query {
            let from_path = format!("{}.{}", template_name, ds.name);
            for u in extract_query_usages(q) {
                out.insert((from_path.clone(), u.object_ref));
            }
        }
        collect_edges(&ds.items, template_name, out);
    }
}

/// Снести строки одной схемы и её рёбра `dcs_query` по полному имени макета.
pub(crate) fn delete_dcs_for_template(
    conn: &rusqlite::Connection,
    template_full_name: &str,
) -> Result<()> {
    let (owner, name) = split_template_full_name(template_full_name);
    conn.execute(
        "DELETE FROM dcs_schemas WHERE repo = ?1 AND template_full_name = ?2",
        params![REPO_DEFAULT, template_full_name],
    )?;
    conn.execute(
        "DELETE FROM dcs_datasets WHERE repo = ?1 AND template_full_name = ?2",
        params![REPO_DEFAULT, template_full_name],
    )?;
    conn.execute(
        "DELETE FROM data_links \
         WHERE repo = ?1 AND link_kind = 'dcs_query' AND from_object = ?2 \
           AND (from_path = ?3 OR from_path LIKE ?4 ESCAPE '\\')",
        params![
            REPO_DEFAULT,
            &owner,
            &name,
            format!("{}.%", like_escape(&name))
        ],
    )?;
    Ok(())
}

/// Снести строки `dcs_*` объекта-владельца (или самого общего макета).
/// Рёбра `dcs_query` не трогаем: при удалении объекта их уносит существующий
/// каскад (`delete_object_cascade` чистит `data_links` в обе стороны).
pub(crate) fn delete_dcs_for_owner(
    conn: &rusqlite::Connection,
    owner_full_name: &str,
) -> Result<()> {
    let like = format!("{}.Template.%", like_escape(owner_full_name));
    conn.execute(
        "DELETE FROM dcs_schemas \
         WHERE repo = ?1 AND (owner_full_name = ?2 OR template_full_name LIKE ?3 ESCAPE '\\')",
        params![REPO_DEFAULT, owner_full_name, &like],
    )?;
    conn.execute(
        "DELETE FROM dcs_datasets \
         WHERE repo = ?1 AND (template_full_name = ?2 OR template_full_name LIKE ?3 ESCAPE '\\')",
        params![REPO_DEFAULT, owner_full_name, &like],
    )?;
    Ok(())
}

/// Фаза полного прохода: пересобрать `dcs_schemas` / `dcs_datasets` и рёбра
/// `dcs_query` всего репо. Идемпотентно (DELETE repo + полный пересбор).
pub(crate) fn index_dcs_schemas(
    repo_root: &Path,
    edt_src: Option<&Path>,
    conn: &rusqlite::Connection,
) -> Result<()> {
    let _ = conn.execute("ROLLBACK", []);
    conn.execute("BEGIN", [])?;
    conn.execute(
        "DELETE FROM dcs_schemas WHERE repo = ?1",
        params![REPO_DEFAULT],
    )?;
    conn.execute(
        "DELETE FROM dcs_datasets WHERE repo = ?1",
        params![REPO_DEFAULT],
    )?;
    conn.execute(
        "DELETE FROM data_links WHERE repo = ?1 AND link_kind = 'dcs_query'",
        params![REPO_DEFAULT],
    )?;

    let targets = dcs_targets(repo_root, edt_src, conn);
    let mut sets = 0usize;
    let mut edges = 0usize;
    for t in &targets {
        match rebuild_dcs_schema(conn, t) {
            Ok((s, e)) => {
                sets += s;
                edges += e;
            }
            Err(e) => tracing::warn!("dcs {}: {}", t.template_full_name, e),
        }
    }
    let schemas: i64 = conn.query_row(
        "SELECT COUNT(*) FROM dcs_schemas WHERE repo = ?1",
        params![REPO_DEFAULT],
        |r| r.get(0),
    )?;
    conn.execute("COMMIT", [])?;

    code_index_core::logging::stage_detail(code_index_core::logging::plural(
        schemas as u64,
        "схема",
        "схемы",
        "схем",
    ));
    tracing::info!(
        "dcs_schemas: {} схем, {} наборов, {} рёбер",
        schemas,
        sets,
        edges
    );
    Ok(())
}

/// Вид макета из описания формата Конфигуратора (`<TemplateType>…</TemplateType>`).
fn config_template_type(content: &str) -> Option<String> {
    let open = content.find("<TemplateType")?;
    let after = content[open..].find('>')? + open;
    let end = content[after..].find("</TemplateType>")? + after;
    let t = content[after + 1..end].trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Пересобрать `dcs_*` макета формата Конфигуратора по пути его ОПИСАНИЯ
/// (`<Вид>/<Объект>/Templates/<Имя>.xml`), если макет — схема компоновки и
/// файл содержимого на месте. Файла нет или макет другого вида — no-op.
///
/// Описание макета читаем сами: паспорт в перечне (`attributes_json`) несёт
/// вид и путь содержимого, но к моменту вызова строки паспорта могло ещё не
/// быть (вставка идёт следом), а снос `dcs_*` обязан произойти в любом случае.
pub(crate) fn rebuild_dcs_for_config_descriptor(
    repo_root: &Path,
    conn: &rusqlite::Connection,
    descriptor: &Path,
    template_full_name: &str,
) -> Result<()> {
    let Ok(content) = std::fs::read_to_string(descriptor) else {
        return Ok(());
    };
    if config_template_type(&content).as_deref() != Some("DataCompositionSchema") {
        return Ok(());
    }
    let (Some(dir), Some(stem)) = (
        descriptor.parent(),
        descriptor.file_stem().and_then(|s| s.to_str()),
    ) else {
        return Ok(());
    };
    let Some(abs) = template_content_in_dir(&dir.join(stem).join("Ext")) else {
        return Ok(());
    };
    let (owner, _name) = split_template_full_name(template_full_name);
    let target = DcsTarget {
        template_full_name: template_full_name.to_string(),
        owner_full_name: owner,
        content_rel: rel_path(repo_root, &abs),
        content_abs: abs,
    };
    rebuild_dcs_schema(conn, &target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_template_full_name_object_and_common() {
        assert_eq!(
            split_template_full_name("Report.Отчет.Template.Схема"),
            ("Report.Отчет".to_string(), "Схема".to_string())
        );
        assert_eq!(
            split_template_full_name("CommonTemplate.ОбщаяСхема"),
            (
                "CommonTemplate.ОбщаяСхема".to_string(),
                "ОбщаяСхема".to_string()
            )
        );
    }
}
