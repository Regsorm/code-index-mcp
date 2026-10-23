//! Resolve subscription source descriptions without reading the working tree.
use std::collections::{BTreeSet, HashMap};

use rusqlite::Connection;
use serde_json::Value;

use crate::xml::event_subscriptions::SourceTypes;

pub(super) fn is_extension(owner: &str) -> bool {
    owner
        .split('/')
        .any(|p| p.eq_ignore_ascii_case("extensions") || p.eq_ignore_ascii_case("cfe"))
}

fn normalized(value: &str) -> String {
    let value = value.trim().trim_start_matches("cfg:");
    if let Some(name) = value.strip_prefix("ОпределяемыйТип.") {
        return format!("definedtype.{}", name.to_lowercase());
    }
    crate::code_usages::normalize_object_ref(value).to_lowercase()
}

fn family(kind: &str) -> (&str, Option<&str>) {
    if kind == "constantvaluemanager" {
        return ("constant", Some("manager"));
    }
    for suffix in ["recordset", "manager", "object", "ref"] {
        if let Some(base) = kind.strip_suffix(suffix) {
            return (base, Some(suffix));
        }
    }
    (kind, None)
}

fn known_family(kind: &str) -> bool {
    matches!(
        kind,
        "document"
            | "catalog"
            | "informationregister"
            | "accumulationregister"
            | "accountingregister"
            | "calculationregister"
            | "chartofaccounts"
            | "chartofcharacteristictypes"
            | "chartofcalculationtypes"
            | "exchangeplan"
            | "businessprocess"
            | "task"
            | "documentjournal"
            | "enum"
            | "constant"
            | "report"
            | "dataprocessor"
            | "sequence"
            | "filtercriterion"
            | "recalculation"
    )
}

pub(super) fn exact_matches(entry: &str, filter: &str) -> bool {
    let entry = normalized(entry);
    let filter = normalized(filter);
    let Some((kind, name)) = entry.split_once('.') else {
        return false;
    };
    let (base, role) = family(kind);
    let Some((wanted, wanted_name)) = filter.split_once('.') else {
        return name == filter && role != Some("ref");
    };
    let (wanted_base, wanted_role) = family(wanted);
    name == wanted_name
        && base == wanted_base
        && (wanted_role.is_none() || wanted_role == role)
        && (role != Some("ref") || wanted_role == Some("ref"))
}

pub(super) struct SourceResolver {
    filter: String,
    kind: Option<String>,
    name: String,
    definitions: HashMap<(String, String), SourceTypes>,
}

#[derive(Clone)]
pub(super) struct SourceMatch {
    pub via: &'static str,
    pub source: String,
    pub defined_type: Option<String>,
}

impl SourceResolver {
    pub fn load(conn: &Connection, filter: &str) -> anyhow::Result<Self> {
        let filter = normalized(filter);
        let (mut kind, name) = filter
            .split_once('.')
            .map(|(t, n)| (Some(t.to_string()), n.to_string()))
            .unwrap_or((None, filter.clone()));
        // An unqualified name can match a universal set only when its metadata
        // kind is unambiguous. Never guess Document for a same-named catalog.
        if kind.is_none() {
            let mut stmt =
                conn.prepare("SELECT meta_type,name FROM metadata_objects WHERE repo='default'")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut kinds = BTreeSet::new();
            for row in rows {
                let (t, n) = row?;
                if n.to_lowercase() == name {
                    kinds.insert(t.to_lowercase());
                }
            }
            if kinds.len() == 1 {
                kind = kinds.into_iter().next();
            }
        }
        let mut definitions = HashMap::new();
        let mut stmt = conn.prepare(
            "SELECT owner,name,types_json FROM subscription_defined_types WHERE repo='default'",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (owner, name, types) = row?;
            definitions.insert((owner, name.to_lowercase()), serde_json::from_str(&types)?);
        }
        // EDT already keeps its root type description in the object passport.
        if definitions.is_empty() {
            let mut stmt = conn.prepare("SELECT name,attributes_json FROM metadata_objects WHERE repo='default' AND meta_type='DefinedType'")?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })?;
            for row in rows {
                let (name, json) = row?;
                let Some(json) = json else {
                    continue;
                };
                let value: Value = serde_json::from_str(&json)?;
                if let Some(types) = value.get("value_types").and_then(Value::as_array) {
                    definitions
                        .entry((String::new(), name.to_lowercase()))
                        .or_insert_with(|| SourceTypes {
                            sources: types
                                .iter()
                                .filter_map(Value::as_str)
                                .map(String::from)
                                .collect(),
                            ..SourceTypes::default()
                        });
                }
            }
        }
        Ok(Self {
            filter,
            kind,
            name,
            definitions,
        })
    }

    pub fn matches(
        &self,
        types: &SourceTypes,
        owner: &str,
        exact_only: bool,
        warnings: &mut BTreeSet<String>,
    ) -> Option<SourceMatch> {
        warnings.extend(types.warnings.iter().cloned());
        let mut best = None;
        for (value, is_set) in types
            .sources
            .iter()
            .map(|s| (s, false))
            .chain(types.type_sets.iter().map(|s| (s, true)))
        {
            if exact_only && (is_set || normalized(value).starts_with("definedtype.")) {
                continue;
            }
            let matched = self.match_value(value, is_set, owner, &mut BTreeSet::new(), warnings);
            if let Some(matched) = matched {
                if exact_only && matched.via != "exact" {
                    continue;
                }
                let rank = |m: &SourceMatch| match m.via {
                    "exact" => 0,
                    "type_set" => 1,
                    _ => 2,
                };
                if best.as_ref().is_none_or(|b| rank(&matched) < rank(b)) {
                    best = Some(matched);
                }
            }
        }
        best
    }

    fn match_value(
        &self,
        value: &str,
        is_set: bool,
        owner: &str,
        seen: &mut BTreeSet<(String, String)>,
        warnings: &mut BTreeSet<String>,
    ) -> Option<SourceMatch> {
        let entry = normalized(value);
        if let Some(name) = entry.strip_prefix("definedtype.") {
            let local_key = (owner.to_string(), name.to_string());
            let key = if self.definitions.contains_key(&local_key) {
                Some(local_key)
            } else {
                let candidates: Vec<_> = self
                    .definitions
                    .keys()
                    .filter(|(o, n)| n == name && !is_extension(o))
                    .cloned()
                    .collect();
                // Prefer the EDT/single-config key over a duplicate passport fallback.
                if candidates.len() == 1 {
                    candidates.first().cloned()
                } else if candidates.iter().any(|(o, _)| o.is_empty()) {
                    Some((String::new(), name.to_string()))
                } else {
                    None
                }
            };
            let Some(key) = key else {
                warnings.insert(format!("unresolved or ambiguous defined type: {value}"));
                return None;
            };
            if seen.len() >= 32 || !seen.insert(key.clone()) {
                warnings.insert(format!("cyclic or too deeply nested defined type: {value}"));
                return None;
            }
            let types = &self.definitions[&key];
            warnings.extend(types.warnings.iter().cloned());
            let mut matched = false;
            for (member, set) in types
                .sources
                .iter()
                .map(|s| (s, false))
                .chain(types.type_sets.iter().map(|s| (s, true)))
            {
                matched |= self
                    .match_value(member, set, &key.0, seen, warnings)
                    .is_some();
            }
            seen.remove(&key);
            return matched.then(|| SourceMatch {
                via: "defined_type",
                source: value.to_string(),
                defined_type: Some(value.trim_start_matches("cfg:").to_string()),
            });
        }
        let (entry_kind, entry_name) = entry
            .split_once('.')
            .map(|(t, n)| (t, Some(n)))
            .unwrap_or((&entry, None));
        let (base, role) = family(entry_kind);
        if !known_family(base) {
            if value.starts_with("cfg:") {
                warnings.insert(format!("unsupported source kind: {entry_kind}"));
            }
            return None;
        }
        if entry_name.is_some() && !is_set {
            return exact_matches(value, &self.filter).then(|| SourceMatch {
                via: "exact",
                source: value.to_string(),
                defined_type: None,
            });
        }
        // TypeSet may also name one explicit type.
        if entry_name.is_some_and(|n| n != self.name) {
            return None;
        }
        let Some(wanted) = self.kind.as_deref() else {
            warnings.insert("use a qualified source (for example Document.Name) to resolve type sets unambiguously".to_string());
            return None;
        };
        let (wanted_base, wanted_role) = family(wanted);
        (base == wanted_base
            && (wanted_role.is_none() || wanted_role == role)
            && (role != Some("ref") || wanted_role == Some("ref")))
        .then(|| SourceMatch {
            via: "type_set",
            source: value.to_string(),
            defined_type: None,
        })
    }
}
