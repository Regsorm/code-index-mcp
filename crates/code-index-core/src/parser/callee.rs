//! Извлечение ИМЕНИ вызываемой функции из узла-выражения.
//!
//! Парсеры писали в `calls.callee` текст всего узла-функции. Для цепочек и
//! квалифицированных путей туда попадал кусок исходника вместе с аргументами
//! и переносами строк:
//!
//! ```text
//! callee = "Self::compress_content(&record.content)\r\n            .context"
//! callee = "json.dumps(payload).encode"
//! ```
//!
//! Поиск по графу вызовов (`get_callers`, `get_callees`, `find_path`,
//! `get_call_tree`) сверяет имя точным равенством, поэтому такие рёбра не
//! находились никогда. На собственном репозитории проекта голым именем были
//! записаны лишь 22 % рёбер, остальные для поиска не существовали.
//!
//! Здесь имя берётся по УЗЛУ, а не обрезкой текста: у выражения
//! `json.dumps(payload).encode()` вызывается `encode`, а `dumps` попадёт в
//! граф своим собственным узлом-вызовом.

use tree_sitter::Node;

/// Откуда брать имя у составного узла: вид узла → имя поля с именем.
/// Один список на все языки — виды узлов у грамматик tree-sitter различаются
/// написанием, но не смыслом.
const NAME_FIELD_BY_KIND: &[(&str, &str)] = &[
    ("attribute", "attribute"),           // Python: obj.method
    ("member_expression", "property"),    // JavaScript/TypeScript: obj.method
    ("member_access_expression", "name"), // C#: obj.Method
    ("member_call_expression", "name"),   // PHP: $obj->method()
    ("scoped_call_expression", "name"),   // PHP: Cls::method()
    ("field_expression", "field"),        // Rust: a.b; C: s.f и s->f
    ("scoped_identifier", "name"),        // Rust: Self::method, Java: p.q.Cls
    ("generic_function", "function"),     // Rust: parse::<T>
    ("selector_expression", "field"),     // Go: pkg.Func
    ("qualified_name", "name"),           // Java, PHP: Ns\func
    ("navigation_expression", "suffix"),  // Swift: obj.method
    ("navigation_suffix", "suffix"),      // Swift: хвост navigation_expression
];

/// Узлы-обёртки, у которых имя лежит глубже и берётся без выбора поля.
fn unwrap_transparent(node: Node) -> Option<Node> {
    match node.kind() {
        "parenthesized_expression" | "non_null_expression" => node.named_child(0),
        _ => None,
    }
}

/// Предел спусков по узлу — страховка от неожиданной формы дерева.
/// Реальные квалификаторы короткие: `a::b::c` — три шага.
const MAX_UNWRAP_STEPS: usize = 16;

/// Имя вызываемой функции из узла-выражения в позиции вызываемого.
///
/// `None` — имени нет: вызывается результат выражения (`handlers[i]()`,
/// `(*callback)()`, немедленно вызванная функция). Прежде на месте имени
/// оказывался текст выражения, который всё равно не находился поиском.
pub fn callee_name(node: Node, source: &[u8]) -> Option<String> {
    let mut cur = node;

    for _ in 0..MAX_UNWRAP_STEPS {
        if let Some(inner) = unwrap_transparent(cur) {
            cur = inner;
            continue;
        }
        match NAME_FIELD_BY_KIND
            .iter()
            .find(|(kind, _)| *kind == cur.kind())
            .map(|(_, field)| *field)
        {
            Some(field) => match cur.child_by_field_name(field) {
                Some(next) => cur = next,
                None => return None,
            },
            None => break,
        }
    }

    let text = std::str::from_utf8(&source[cur.byte_range()]).ok()?;
    is_plain_name(text).then(|| text.to_string())
}

/// Годится ли строка на роль имени функции: один идентификатор без скобок,
/// пробелов и переносов. Юникод допускается — имена методов бывают
/// нелатинскими. Хвостовые `?` и `!` — законная часть имени в Ruby.
fn is_plain_name(text: &str) -> bool {
    let core = text.trim_end_matches(['?', '!']);
    !core.is_empty()
        && core
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// Годится ли строка на роль квалификатора `Модуль.Метод` (BSL): цепочка имён
/// через точку, без скобок, индексов и переносов строк.
pub fn is_plain_qualifier(text: &str) -> bool {
    !text.is_empty() && text.split('.').all(is_plain_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_name_accepts_identifiers() {
        assert!(is_plain_name("compress_content"));
        assert!(is_plain_name("ЗначениеРеквизита"));
        assert!(is_plain_name("empty?"));
        assert!(is_plain_name("$fn"));
    }

    #[test]
    fn plain_name_rejects_expressions() {
        assert!(!is_plain_name("Self::compress_content(&x)"));
        assert!(!is_plain_name("json.dumps"));
        assert!(!is_plain_name("f(1)\n    .context"));
        assert!(!is_plain_name(""));
    }

    #[test]
    fn plain_qualifier_accepts_dotted_names() {
        assert!(is_plain_qualifier("ОбщегоНазначения"));
        assert!(is_plain_qualifier("Справочники.Номенклатура"));
        assert!(!is_plain_qualifier("Ф(1)"));
        assert!(!is_plain_qualifier("Массив[0]"));
    }
}
