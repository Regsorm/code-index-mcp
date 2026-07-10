//! Единый слой разбора BSL, которым пользуются и валидатор (`bsl-validator`),
//! и внешний индексатор кода: нормализация исходника под дефекты грамматики
//! `tree-sitter-bsl`, разбор дерева и сбор фактов (`collect_facts`), а также
//! текстовая маскировка строк/комментариев и объявлений процедур/функций.
//! Здесь нет ничего про платформенный контекст 1С — крейт не зависит от
//! `platform-index`.

use std::collections::HashSet;

/// Голый вызов `Имя(...)` — не метод объекта.
pub struct CallFact {
    pub name: String,
    pub arg_count: usize,
    /// Начало идентификатора в БАЙТАХ исходного текста (для pos_at и scope_map).
    pub byte: usize,
}

/// Обращение `Голова.Член` — свойство, значение перечисления или метод объекта.
pub struct DotFact {
    pub head: String,
    pub member: String,
    pub head_byte: usize,
    pub member_byte: usize,
}

/// Конструктор `Новый ИмяТипа`.
pub struct NewFact {
    pub type_name: String,
    pub byte: usize,
}

#[derive(Default)]
pub struct AstFacts {
    /// Имена объявленных процедур/функций в нижнем регистре.
    pub declarations: HashSet<String>,
    pub calls: Vec<CallFact>,
    pub dots: Vec<DotFact>,
    pub news: Vec<NewFact>,
    /// false — дерево получить не удалось (двоичный модуль, таймаут, сбой языка).
    /// Сегодня отдельно не проверяется: пустое дерево и так даёт пустые facts,
    /// проверки над ними естественно молчат. Поле — задел для вызывающего кода,
    /// которому важно различить «кода нет ошибок» и «дерево не разобралось».
    pub parsed: bool,
}

/// Обход трёх дефектов грамматики `tree-sitter-bsl` 0.1.7 (последняя доступная;
/// у автора открыт issue #7 «parenthesized expressions cause parse errors»).
/// Все замены выполняются ПОБАЙТНО и сохраняют длину, поэтому смещения узлов
/// совпадают с оригиналом — тексты имён мы читаем из исходного текста, а не из
/// нормализованного.
///
/// 1. **Буква `ё`.** Идентификатор описан как `/[\wа-я_][\wа-я_0-9]*/i`, а `ё`
///    (U+0451) в диапазон `а-я` не входит. Имя `СчётаУчёта` рвётся на куски,
///    объявление теряется. Меняем `ё`→`е`, `Ё`→`Е` (обе пары двухбайтные).
///
/// 2. **Тернарный оператор с пробелом.** `? (Усл, А, Б)` уходит в `ERROR`, хотя
///    `?(Усл, А, Б)` разбирается. Переносим пробелы за скобку: `?( Усл, А, Б)`.
///
/// 3. **`ВызватьИсключение;` без аргумента.** Разбирается, только когда стоит
///    первым в теле `Исключение`; внутри `Если`/цикла — `ERROR`. Затираем само
///    слово пробелами: остаётся пустой оператор `;`, для наших фактов он пуст.
///
/// 4. **Неразрывный пробел** (U+00A0) в отступах — грамматика не считает его
///    пробелом. Занимает 2 байта (`C2 A0`), меняем на два обычных пробела.
///
/// 5. **`# Если`** с пробелом после решётки: `#Если` разбирается, `# Если` — нет.
///    Переносим пробелы за имя директивы, как в случае с тернарным оператором.
///
/// 6. **Отрицательное значение параметра по умолчанию** — `Процедура П(А = -1)`.
///    Минус в заголовке меняем на пробел (1 байт → 1 байт). Значения по умолчанию
///    в фактах не используются, поэтому смысл разбора не страдает.
///
/// Остаётся один дефект, который так обойти НЕЛЬЗЯ (длина изменится): обращение
/// к результату тернарного оператора — `?(У, А, Б).Метод()`. Он даёт локальный
/// `ERROR`, объявления и вызовы вокруг не теряются, а сам метод обезвреживается
/// в `collect_facts` проверкой точки слева от вызова, иначе он выглядел бы
/// глобальной функцией.
///
/// Если ни один случай не встретился, копия не создаётся.
///
/// Публична, потому что тот же текст должен подавать парсеру любой внешний
/// потребитель этой грамматики (индексатор кода), иначе он унаследует все три
/// дефекта.
pub fn normalize_for_parser(source: &str) -> std::borrow::Cow<'_, str> {
    let bytes = source.as_bytes();
    let has_yo = bytes
        .windows(2)
        .any(|w| w == [0xD1, 0x91] || w == [0xD0, 0x81]);
    let has_nbsp = bytes.windows(2).any(|w| w == [0xC2, 0xA0]);
    let has_ternary_gap = find_ternary_gap(bytes).is_some();
    let has_bare_raise = find_bare_raise(bytes, 0).is_some();
    let has_preproc_gap = find_preproc_gap(bytes, 0).is_some();
    let has_neg_default = !negative_defaults(bytes).is_empty();
    if !has_yo
        && !has_nbsp
        && !has_ternary_gap
        && !has_bare_raise
        && !has_preproc_gap
        && !has_neg_default
    {
        return std::borrow::Cow::Borrowed(source);
    }

    let mut out = bytes.to_vec();

    // ── 1. ё → е, Ё → Е;  4. неразрывный пробел → два обычных
    let mut i = 0;
    while i + 1 < out.len() {
        match (out[i], out[i + 1]) {
            (0xD1, 0x91) => {
                out[i] = 0xD0;
                out[i + 1] = 0xB5;
                i += 2;
            }
            (0xD0, 0x81) => {
                out[i] = 0xD0;
                out[i + 1] = 0x95;
                i += 2;
            }
            (0xC2, 0xA0) => {
                out[i] = b' ';
                out[i + 1] = b' ';
                i += 2;
            }
            _ => i += 1,
        }
    }

    // ── 2. `?` + пробелы/табы + `(`  →  `?(` + те же пробелы/табы
    let mut from = 0;
    while let Some((q, open)) = find_ternary_gap_from(&out, from) {
        out[q + 1] = b'(';
        for b in out.iter_mut().take(open + 1).skip(q + 2) {
            *b = b' ';
        }
        from = open + 1;
    }

    // ── 3. `ВызватьИсключение` / `Raise`, за которыми сразу `;` → пробелы
    let mut from = 0;
    while let Some((start, end)) = find_bare_raise(&out, from) {
        for b in out.iter_mut().take(end).skip(start) {
            *b = b' ';
        }
        from = end;
    }

    // ── 5. `#` + пробелы + буква  →  `#` + буква … пробелы уходят за слово
    let mut from = 0;
    while let Some((hash, word_start, word_end)) = find_preproc_gap(&out, from) {
        let gap = word_start - hash - 1;
        out.copy_within(word_start..word_end, hash + 1);
        for b in out.iter_mut().take(word_end).skip(word_end - gap) {
            *b = b' ';
        }
        from = word_end;
    }

    // ── 6. `= -1` в заголовке процедуры/функции → минус меняем на пробел
    for pos in negative_defaults(&out) {
        out[pos] = b' ';
    }

    std::borrow::Cow::Owned(String::from_utf8(out).expect("побайтные замены сохраняют UTF-8"))
}

/// Позиции минусов в отрицательных значениях параметров по умолчанию.
///
/// Идём от РЕДКИХ кандидатов: минус, слева от которого через пробелы `=`,
/// а справа цифра. Только для них проверяем, что место — список параметров
/// заголовка `Процедура|Функция Имя( … )`. Обратный порядок (искать заголовки,
/// потом минусы внутри) сканировал весь текст и стоил дороже самого разбора.
fn negative_defaults(bytes: &[u8]) -> Vec<usize> {
    /// Заголовок процедуры не бывает длиннее — дальше назад не смотрим.
    const HEADER_LOOKBACK: usize = 4096;

    let mut out = Vec::new();
    for i in 0..bytes.len() {
        if bytes[i] != b'-' {
            continue;
        }
        // справа — цифра?
        let mut d = i + 1;
        while d < bytes.len() && (bytes[d] == b' ' || bytes[d] == b'\t') {
            d += 1;
        }
        if d >= bytes.len() || !bytes[d].is_ascii_digit() {
            continue;
        }
        // слева — знак `=`?
        let mut e = i;
        while e > 0 && (bytes[e - 1] == b' ' || bytes[e - 1] == b'\t') {
            e -= 1;
        }
        if e == 0 || bytes[e - 1] != b'=' {
            continue;
        }
        if in_procedure_header(bytes, e - 1, HEADER_LOOKBACK) {
            out.push(i);
        }
    }
    out
}

/// Позиция `pos` находится внутри списка параметров заголовка процедуры/функции?
/// Идём назад до непарной `(`, затем проверяем «имя» и ключевое слово перед ним.
fn in_procedure_header(bytes: &[u8], pos: usize, lookback: usize) -> bool {
    let stop = pos.saturating_sub(lookback);
    let mut depth = 0i32;
    let mut i = pos;
    let open = loop {
        if i == stop {
            return false;
        }
        i -= 1;
        match bytes[i] {
            b')' => depth += 1,
            b'(' => {
                if depth == 0 {
                    break i;
                }
                depth -= 1;
            }
            // до заголовка эти символы встретиться не должны
            b';' | b'}' => return false,
            _ => {}
        }
    };

    // назад от `(`: пробелы, имя процедуры, пробелы, ключевое слово
    let mut j = open;
    while j > 0 && (bytes[j - 1] == b' ' || bytes[j - 1] == b'\t') {
        j -= 1;
    }
    while j > 0 && is_ident_byte(bytes[j - 1]) {
        j -= 1;
    }
    while j > 0 && (bytes[j - 1] == b' ' || bytes[j - 1] == b'\t') {
        j -= 1;
    }
    for kw in ["процедура", "функция", "procedure", "function"] {
        let len = kw.len();
        if j >= len && kw_ends_at(bytes, j - len, kw).is_some() {
            return true;
        }
    }
    false
}

/// `#`, за которым идут пробелы/табы, а потом буква: `# Если`, `# Область`.
/// Возвращает `(позиция #, начало слова, конец слова)`.
fn find_preproc_gap(bytes: &[u8], from: usize) -> Option<(usize, usize, usize)> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j > i + 1 && j < bytes.len() && is_ident_byte(bytes[j]) {
                let mut k = j;
                while k < bytes.len() && is_ident_byte(bytes[k]) {
                    k += 1;
                }
                return Some((i, j, k));
            }
        }
        i += 1;
    }
    None
}

fn find_ternary_gap(bytes: &[u8]) -> Option<(usize, usize)> {
    find_ternary_gap_from(bytes, 0)
}

/// Позиции `?` и следующей за пробелами `(`. Только если между ними есть хотя бы
/// один пробел или таб — иначе чинить нечего.
fn find_ternary_gap_from(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'?' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j > i + 1 && j < bytes.len() && bytes[j] == b'(' {
                return Some((i, j));
            }
        }
        i += 1;
    }
    None
}

/// Границы слова `ВызватьИсключение`/`Raise`, за которым (через пробелы) сразу
/// идёт `;`. Форма с аргументом (`ВызватьИсключение "текст";`) грамматике понятна
/// и не трогается. Регистр значения не имеет — в BSL он не различается.
fn find_bare_raise(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    // Отсев по первой букве: `В` = D0 92, `в` = D0 B2, плюс ASCII `R`/`r`.
    // Без него регистронезависимое сравнение звалось бы на каждом байте
    // кириллицы — это и был главный источник замедления нормализации.
    let mut i = from;
    while i < bytes.len() {
        let b = bytes[i];
        let maybe_ru = b == 0xD0 && bytes.get(i + 1).is_some_and(|&c| c == 0x92 || c == 0xB2);
        let maybe_en = b == b'R' || b == b'r';
        if maybe_ru || maybe_en {
            for kw in ["вызватьисключение", "raise"] {
                let Some(end) = kw_ends_at(bytes, i, kw) else { continue };
                if !on_word_boundary(bytes, i, end) {
                    continue;
                }
                let mut j = end;
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b';' {
                    return Some((i, end));
                }
            }
        }
        i += 1;
    }
    None
}

/// Конец слова, если срез с позиции `i` регистронезависимо равен `kw_lower`.
/// Сравнение побайтное, без аллокаций: `to_lowercase()` на каждом кандидате
/// стоил вдвое дороже всего остального разбора.
fn kw_ends_at(bytes: &[u8], i: usize, kw_lower: &str) -> Option<usize> {
    let kw = kw_lower.as_bytes();
    let end = i + kw.len();
    if end > bytes.len() {
        return None;
    }
    let mut a = i;
    let mut b = 0;
    while b < kw.len() {
        if kw[b] < 0x80 {
            if !bytes[a].eq_ignore_ascii_case(&kw[b]) {
                return None;
            }
            a += 1;
            b += 1;
        } else {
            // кириллица: два байта, приводим исходник к нижнему регистру
            let (l1, l2) = lower_cyrillic(bytes[a], *bytes.get(a + 1)?);
            if l1 != kw[b] || l2 != *kw.get(b + 1)? {
                return None;
            }
            a += 2;
            b += 2;
        }
    }
    Some(end)
}

/// Нижний регистр для двухбайтной кириллицы UTF-8.
/// `А`-`П` = D0 90..9F → +0x20; `Р`-`Я` = D0 A0..AF → D1, −0x20.
fn lower_cyrillic(b1: u8, b2: u8) -> (u8, u8) {
    match (b1, b2) {
        (0xD0, 0x90..=0x9F) => (0xD0, b2 + 0x20),
        (0xD0, 0xA0..=0xAF) => (0xD1, b2 - 0x20),
        _ => (b1, b2),
    }
}

/// Слово стоит на границе: слева и справа не буква/цифра/подчёркивание.
fn on_word_boundary(bytes: &[u8], start: usize, end: usize) -> bool {
    let left_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
    let right_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
    left_ok && right_ok
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Слева от `start` (через пробелы и переводы строк) стоит точка?
fn preceded_by_dot(src: &[u8], start: usize) -> bool {
    let mut i = start;
    while i > 0 {
        match src[i - 1] {
            b' ' | b'\t' | b'\r' | b'\n' => i -= 1,
            b'.' => return true,
            _ => return false,
        }
    }
    false
}

/// Слева от `start` (через пробелы и переводы строк) стоит слово `Новый`/`New`?
///
/// Обычно конструктор виден по узлу `new_expression`, и текстовая проверка не
/// нужна. Но если рядом стоит конструкция, которой грамматика не знает (например
/// `#Если` внутри списка аргументов), узел разваливается, и `Новый Тип(...)`
/// приходит к нам обычным вызовом. Тогда спасает только слово слева.
fn preceded_by_new(src: &[u8], start: usize) -> bool {
    let mut i = start;
    while i > 0 && matches!(src[i - 1], b' ' | b'\t' | b'\r' | b'\n') {
        i -= 1;
    }
    for kw in ["новый", "new"] {
        let len = kw.len();
        if i >= len && kw_ends_at(src, i - len, kw).is_some() && on_word_boundary(src, i - len, i) {
            return true;
        }
    }
    false
}

/// Разобрать `source` деревом tree-sitter-bsl и одним проходом собрать факты
/// для всех проверок уровня 1: объявления процедур/функций, голые вызовы,
/// обращения через точку и конструкторы `Новый`.
pub fn collect_facts(source: &str) -> AstFacts {
    // Двоичный .bsl (EDT-защищённые модули поставщика) — не отдаём в
    // tree-sitter, иначе он деградирует на бесструктурном вводе. Маркер —
    // NUL-байт в первых 8 КБ (см. `code-index::parser::bsl::looks_binary`).
    if source.as_bytes().iter().take(8192).any(|&b| b == 0) {
        return AstFacts::default();
    }

    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_bsl::LANGUAGE.into())
        .is_err()
    {
        // Не смогли выставить язык — молча возвращаем пустые факты.
        return AstFacts::default();
    }
    // Страховка от патологического ввода: 10-секундный дедлайн парсинга.
    #[allow(deprecated)]
    parser.set_timeout_micros(10_000 * 1000);

    // Разбираем нормализованный текст, читаем — оригинальный: смещения совпадают.
    let for_parser = normalize_for_parser(source);
    let Some(tree) = parser.parse(for_parser.as_ref(), None) else {
        return AstFacts::default();
    };

    let src = source.as_bytes();
    let mut facts = AstFacts {
        parsed: true,
        ..Default::default()
    };

    // Итеративный обход: Vec как стек вместо рекурсии — модули конфигурации
    // достигают десятков тысяч строк, а глубина AST у них непредсказуема.
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "procedure_definition" | "function_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if let Ok(name) = name_node.utf8_text(src) {
                        facts.declarations.insert(name.to_lowercase());
                    }
                }
            }
            "new_expression" => {
                let mut cursor = node.walk();
                let ident = node
                    .named_children(&mut cursor)
                    .find(|c| c.kind() == "identifier");
                if let Some(ident) = ident {
                    if let Ok(text) = ident.utf8_text(src) {
                        facts.news.push(NewFact {
                            type_name: text.to_string(),
                            byte: ident.start_byte(),
                        });
                    }
                }
            }
            "property_access" | "access" => {
                // `property_access` — цепочка `Голова.Член`, целиком составляющая
                // выражение. У промежуточного звена более длинной цепочки
                // (`ТЗ.Колонкы.Добавить(...)`) та же форма (голова + `property`),
                // но грамматика алиасит в `property_access` только САМЫЙ внешний
                // сегмент — внутренние остаются простым `access`. Голый
                // `access(identifier)` с одним ребёнком сюда не попадает: без
                // второго именованного ребёнка `find` ниже вернёт `None`.
                //
                // Член звена — либо `property` (`Запрос.Текст`), либо `method_call`
                // (`Запрос.Выполнить().Выбрать()`: внутреннее звено — вызов, а не
                // свойство). Без второго случая опечатка в имени метода внутри
                // цепочки не находилась бы, хотя прежняя проверка её ловила.
                let mut cursor = node.walk();
                let member_node = node
                    .named_children(&mut cursor)
                    .find(|c| matches!(c.kind(), "property" | "method_call"))
                    // У `method_call` именем является его первый ребёнок-identifier.
                    .and_then(|c| if c.kind() == "method_call" { c.child(0) } else { Some(c) });
                if let Some(member_node) = member_node {
                    if let Some((head, head_byte)) = simple_head(node.child(0), src) {
                        if let Ok(member) = member_node.utf8_text(src) {
                            facts.dots.push(DotFact {
                                head,
                                member: member.to_string(),
                                head_byte,
                                member_byte: member_node.start_byte(),
                            });
                        }
                    }
                }
            }
            "call_expression" => {
                if let Some((head, head_byte)) = simple_head(node.child(0), src) {
                    let mut cursor = node.walk();
                    let method_call_node = node
                        .named_children(&mut cursor)
                        .find(|c| c.kind() == "method_call");
                    if let Some(mc) = method_call_node {
                        if let Some(member_node) = mc.child(0) {
                            if let Ok(member) = member_node.utf8_text(src) {
                                facts.dots.push(DotFact {
                                    head,
                                    member: member.to_string(),
                                    head_byte,
                                    member_byte: member_node.start_byte(),
                                });
                            }
                        }
                    }
                }
            }
            "method_call" => {
                // Метод объекта (`Объект.Метод(...)`) уже разобрала ветка
                // `call_expression` выше — здесь его пропускаем, чтобы не
                // задвоить находку и не принять его за голый глобальный вызов.
                //
                // Исключение — ГОЛОВА цепочки (`ПустаяСсылка().Метаданные()`):
                // слева от неё точки нет, это обычный голый вызов. В дереве она
                // лежит внутри `access`, у которого она единственный именованный
                // ребёнок. Без этого исключения любая опечатка в начале цепочки
                // не проверялась бы вовсе.
                let parent = node.parent();
                let mut is_member_call = parent.is_some_and(|p| {
                    matches!(p.kind(), "call_expression" | "access") && p.named_child_count() > 1
                });
                // Страховка от «восстановления» дерева на конструкциях, которые
                // грамматика не понимает. Два случая, оба подтверждены на УТ:
                //
                // 1. `?(У, А, Б).ПолучитьИмена()` — дерево рвёт так, что
                //    `.ПолучитьИмена()` становится ОТДЕЛЬНЫМ оператором вызова с
                //    обычным родителем, и метод результата выглядит глобальной
                //    функцией. Точка слева говорит, что это не так.
                //
                // 2. Директива препроцессора внутри списка аргументов
                //    (`Новый Структура("а,б", Новый ОписаниеТипов(...), #Если … )`)
                //    — грамматика такого не допускает, узел `new_expression`
                //    разваливается, и конструктор выглядит вызовом функции. Слово
                //    `Новый` слева говорит, что это конструктор.
                if !is_member_call {
                    if let Some(id) = node.child(0) {
                        let b = id.start_byte();
                        is_member_call = preceded_by_dot(src, b) || preceded_by_new(src, b);
                    }
                }
                if !is_member_call {
                    if let Some(id_node) = node.child(0) {
                        if let Ok(name) = id_node.utf8_text(src) {
                            facts.calls.push(CallFact {
                                name: name.to_string(),
                                arg_count: count_arguments(node),
                                byte: id_node.start_byte(),
                            });
                        }
                    }
                }
            }
            _ => {}
        }

        // Детей кладём справа налево: `pop()` тогда возвращает их слева направо,
        // и факты собираются в порядке текста, а не задом наперёд.
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }

    facts
}

/// Голова обращения через точку: узел `access`, состоящий ровно из одного
/// `identifier`. Цепочки (`Запрос.Выполнить().Выбрать()`) головой не считаются —
/// их разбирает return-type tracking уровня 3 через scope_map.
fn simple_head<'a>(node: Option<tree_sitter::Node<'a>>, src: &[u8]) -> Option<(String, usize)> {
    let node = node?;
    if node.kind() != "access" || node.named_child_count() != 1 {
        return None;
    }
    let ident = node.named_child(0)?;
    if ident.kind() != "identifier" {
        return None;
    }
    let text = ident.utf8_text(src).ok()?;
    Some((text.to_string(), ident.start_byte()))
}

/// Число аргументов голого вызова `Имя(...)`: именованные дети узла
/// `arguments`, кроме комментариев. Узла `arguments` нет — аргументов 0.
fn count_arguments(method_call: tree_sitter::Node) -> usize {
    let mut cursor = method_call.walk();
    let args = method_call
        .named_children(&mut cursor)
        .find(|c| c.kind() == "arguments");
    let Some(args) = args else {
        return 0;
    };
    let mut cursor = args.walk();
    args.named_children(&mut cursor)
        .filter(|c| c.kind() != "line_comment")
        .count()
}

// ── Очистка строк и комментариев ──────────────────────────────────────────

/// Замаскировать пробелами строковые литералы и комментарии. Длина и позиции
/// строк сохраняются — это важно для line/col, передаваемых в ошибки. Русские
/// буквы и прочие multi-byte UTF-8 символы НЕ трогаются — пробелами заменяются
/// только байты внутри строк/комментариев (ASCII содержимое).
/// Убрать директивы препроцессора расширений, сохранив длину строк.
///
/// В модуле расширения блок `#Удаление … #КонецУдаления` содержит код исходного
/// модуля, который расширение выбрасывает: в скомпилированный модуль он не
/// попадает. Беда в том, что этот код может обрывать строковый литерал на
/// середине — тогда сам файл перестаёт быть корректным BSL, а маскировка строк
/// «съезжает» и весь текст запроса ниже начинает считаться кодом (наблюдалось на
/// `#Удаление` внутри текста запроса: закрывающая кавычка стояла в удаляемой
/// строке, а вставляемая её не имела).
///
/// Поэтому удаляемые строки и сами строки-маркеры затираются пробелами до всякого
/// разбора. Позиции сохраняются: и номера строк, и колонки остаются прежними.
pub fn strip_extension_directives(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = bytes.to_vec();
    let mut in_deleted = false;
    let mut pos = 0usize;

    for line in src.split_inclusive('\n') {
        let body_len = line.trim_end_matches(['\n', '\r']).len();
        let head = line.trim_start().to_lowercase();

        let starts_delete = head.starts_with("#удаление") || head.starts_with("#delete");
        let ends_delete = head.starts_with("#конецудаления") || head.starts_with("#enddelete");
        let is_marker = starts_delete
            || ends_delete
            || head.starts_with("#вставка")
            || head.starts_with("#конецвставки")
            || head.starts_with("#insert")
            || head.starts_with("#endinsert");

        if starts_delete {
            in_deleted = true;
        }
        if is_marker || in_deleted {
            // Затираем побайтно: длина и позиции сохраняются, UTF-8 остаётся валидным.
            for b in out.iter_mut().take(pos + body_len).skip(pos) {
                if *b != b'\t' {
                    *b = b' ';
                }
            }
        }
        if ends_delete {
            in_deleted = false;
        }
        pos += line.len();
    }

    String::from_utf8(out).expect("затирание пробелами сохраняет UTF-8 валидность")
}

pub fn mask_strings_and_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = bytes.to_vec();

    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // Однострочный комментарий //...
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            let mut j = i;
            while j < bytes.len() && bytes[j] != b'\n' {
                if bytes[j] != b'\r' {
                    out[j] = b' ';
                }
                j += 1;
            }
            i = j;
            continue;
        }
        // Строка "..."
        if b == b'"' {
            out[i] = b' '; // открывающая кавычка
            let mut j = i + 1;
            while j < bytes.len() {
                if bytes[j] == b'"' {
                    if j + 1 < bytes.len() && bytes[j + 1] == b'"' {
                        // escaped quote — затираем обе и идём дальше
                        out[j] = b' ';
                        out[j + 1] = b' ';
                        j += 2;
                        continue;
                    }
                    out[j] = b' ';
                    j += 1;
                    break;
                }
                if bytes[j] == b'\n' {
                    // Перевод строки внутри многострочного литерала. Платформа
                    // разрешает вставлять между строками-продолжениями (`|`)
                    // обычные комментарии. Кавычка в таком комментарии литерал
                    // НЕ закрывает — иначе всё, что ниже, инвертируется:
                    // код считается строкой, а текст запроса кодом.
                    j += 1;
                    let mut k = j;
                    while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
                        k += 1;
                    }
                    if k + 1 < bytes.len() && bytes[k] == b'/' && bytes[k + 1] == b'/' {
                        while k < bytes.len() && bytes[k] != b'\n' {
                            if bytes[k] != b'\r' {
                                out[k] = b' ';
                            }
                            k += 1;
                        }
                        j = k;
                    }
                    continue;
                }
                if bytes[j] != b'\r' {
                    out[j] = b' ';
                }
                j += 1;
            }
            i = j;
            continue;
        }
        i += 1;
    }

    String::from_utf8(out).expect("mask_strings_and_comments сохраняет UTF-8 валидность")
}

/// Запасной сбор объявлений построчно — на случай, когда tree-sitter не смог
/// разобрать модуль и часть `proc_declaration`/`func_declaration` потерялась.
///
/// Строки и комментарии предварительно замаскированы, поэтому слово `Процедура`
/// внутри строкового литерала объявлением не станет. Имя берётся до первой
/// открывающей скобки; строки без скобки игнорируются.
pub fn scan_declarations(source: &str) -> HashSet<String> {
    let cleaned = mask_strings_and_comments(source);
    let mut names = HashSet::new();
    for line in cleaned.lines() {
        let trimmed = line.trim_start();
        let lower = trimmed.to_lowercase();
        let rest = ["процедура ", "функция ", "procedure ", "function "]
            .iter()
            .find_map(|kw| lower.strip_prefix(kw));
        let Some(rest) = rest else { continue };
        let Some((name, _)) = rest.split_once('(') else {
            continue;
        };
        let name = name.trim();
        if !name.is_empty() && !name.contains(char::is_whitespace) {
            names.insert(name.to_string());
        }
    }
    names
}

/// Имена процедур и функций, объявленных в модуле (в нижнем регистре).
///
/// Объединение ДВУХ источников: дерева и текстового прохода. Ни один не полон —
/// дерево теряет объявления на файлах с `ERROR`, а текстовый проход не видит
/// заголовков с переносом строки перед скобкой. Платформенный индекс здесь не
/// нужен: это чистый разбор.
///
/// Вынесено в публичный API, потому что тот же список объявлений нужен внешнему
/// индексатору кода (code-index), а не только валидатору.
pub fn module_declarations(source: &str) -> HashSet<String> {
    let (from_ast, from_text) = module_declarations_split(source);
    let mut all = from_ast;
    all.extend(from_text);
    all
}

/// Те же объявления, но раздельно: `(из дерева, из текста)`. Нужно для замеров
/// качества разбора — какой источник что теряет.
pub fn module_declarations_split(source: &str) -> (HashSet<String>, HashSet<String>) {
    let source = &strip_extension_directives(source);
    let facts = collect_facts(source);
    (facts.declarations, scan_declarations(source))
}

/// Объявление процедуры/функции модуля — то, что нужно облегчённому индексу.
#[derive(Debug, Clone, PartialEq)]
pub struct MethodDecl {
    pub name: String,
    pub is_function: bool,
    pub is_export: bool,
    /// Директива компиляции без амперсанда: "НаСервере", "Вместо" и т.п.
    pub directive: Option<String>,
    /// 1-based номер строки объявления.
    pub line_start: u32,
    /// Текст списка параметров вместе со скобками: "(А, Знач Б = 1)".
    pub params: Option<String>,
}

/// Методы, объявленные в модуле. Дерево + нормализация (та же, что в collect_facts).
/// Текстовой страховки здесь НЕТ: она даёт только имена, без строк и параметров.
pub fn collect_methods(source: &str) -> Vec<MethodDecl> {
    // Двоичный .bsl (EDT-защищённые модули поставщика) — не отдаём в
    // tree-sitter, см. collect_facts.
    if source.as_bytes().iter().take(8192).any(|&b| b == 0) {
        return Vec::new();
    }

    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_bsl::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    #[allow(deprecated)]
    parser.set_timeout_micros(10_000 * 1000);

    // Разбираем нормализованный текст, читаем — оригинальный: смещения совпадают.
    let for_parser = normalize_for_parser(source);
    let Some(tree) = parser.parse(for_parser.as_ref(), None) else {
        return Vec::new();
    };

    let src = source.as_bytes();
    let mut methods = Vec::new();

    // Итеративный обход: та же схема, что в collect_facts.
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if let "procedure_definition" | "function_definition" = node.kind() {
            if let Some(name_node) = node.child_by_field_name("name") {
                if let Ok(name) = name_node.utf8_text(src) {
                    let params = node
                        .child_by_field_name("parameters")
                        .and_then(|p| p.utf8_text(src).ok())
                        .map(|s| s.to_string());
                    let is_export = node.child_by_field_name("export").is_some();

                    // Директива компиляции — ПРЕДЫДУЩИЙ СОСЕД узла определения:
                    // `preprocessor`, внутри которого узел `annotation` вида "&НаСервере".
                    let directive = node.prev_sibling().and_then(|prev| {
                        if prev.kind() != "preprocessor" {
                            return None;
                        }
                        let mut cursor = prev.walk();
                        let annotation = prev
                            .named_children(&mut cursor)
                            .find(|c| c.kind() == "annotation")?;
                        annotation
                            .utf8_text(src)
                            .ok()
                            .map(|t| t.trim_start_matches('&').to_string())
                    });

                    methods.push(MethodDecl {
                        name: name.to_string(),
                        is_function: node.kind() == "function_definition",
                        is_export,
                        directive,
                        line_start: node.start_position().row as u32 + 1,
                        params,
                    });
                }
            }
        }

        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }

    methods
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_calls_with_nested_call_argument() {
        let facts = collect_facts("Сообщить(Строка(1));");
        assert!(facts.parsed);
        let names: Vec<&str> = facts.calls.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"Сообщить"), "нет Сообщить: {:?}", names);
        assert!(names.contains(&"Строка"), "нет Строка: {:?}", names);
        for call in &facts.calls {
            assert_eq!(call.arg_count, 1, "аргумент у {}", call.name);
        }
    }

    #[test]
    fn omitted_argument_counts_as_one() {
        let facts = collect_facts("Ф(1, , 3);");
        assert_eq!(facts.calls.len(), 1);
        assert_eq!(facts.calls[0].arg_count, 3);
    }

    #[test]
    fn comma_inside_string_is_not_a_separator() {
        let facts = collect_facts("Ф(\"а,б\", 2);");
        assert_eq!(facts.calls.len(), 1);
        assert_eq!(facts.calls[0].arg_count, 2);
    }

    #[test]
    fn query_text_cast_is_not_a_call() {
        let facts =
            collect_facts("З = Новый Запрос(\"ВЫБРАТЬ ВЫРАЗИТЬ(Т.С КАК ЧИСЛО(15,2))\");");
        assert!(facts.calls.is_empty(), "ЧИСЛО из текста запроса: {:?}", facts.calls.iter().map(|c| &c.name).collect::<Vec<_>>());
        assert_eq!(facts.news.len(), 1);
        assert_eq!(facts.news[0].type_name, "Запрос");
    }

    #[test]
    fn if_keyword_is_not_a_call() {
        let facts = collect_facts("Если Условие(1) Тогда КонецЕсли;");
        let names: Vec<&str> = facts.calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["Условие"]);
    }

    #[test]
    fn constructor_is_not_a_call() {
        let facts = collect_facts("М = Новый Массив(10);");
        assert_eq!(facts.news.len(), 1);
        assert_eq!(facts.news[0].type_name, "Массив");
        assert!(facts.calls.is_empty());
    }

    #[test]
    fn enum_property_access() {
        let facts = collect_facts("Ор = ОриентацияСтраницы.Ландшафт;");
        assert_eq!(facts.dots.len(), 1);
        assert_eq!(facts.dots[0].head, "ОриентацияСтраницы");
        assert_eq!(facts.dots[0].member, "Ландшафт");
    }

    #[test]
    fn object_method_call_is_not_a_bare_call() {
        let facts = collect_facts("ТабДок.Вывести(Рез);");
        assert_eq!(facts.dots.len(), 1);
        assert_eq!(facts.dots[0].head, "ТабДок");
        assert_eq!(facts.dots[0].member, "Вывести");
        assert!(facts.calls.is_empty());
    }

    #[test]
    fn chain_prefix_dot_is_caught_before_trailing_call() {
        // `ТЗ.Колонкы.Добавить(...)`: грамматика алиасит в `property_access`
        // только весь внешний сегмент цепочки, промежуточное звено `ТЗ.Колонкы`
        // остаётся простым узлом `access` — он должен попасть в dots тоже.
        let facts = collect_facts("ТЗ.Колонкы.Добавить(\"Поле\");");
        assert!(
            facts
                .dots
                .iter()
                .any(|d| d.head == "ТЗ" && d.member == "Колонкы"),
            "нет ТЗ.Колонкы: {:?}",
            facts.dots.iter().map(|d| (&d.head, &d.member)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn chain_method_link_is_caught() {
        // `Запрос.Выполнить().Выбрать()`: внутреннее звено — вызов метода, а не
        // свойство. Опечатка в нём (`Выполнть`) должна попадать в dots, иначе
        // проверка члена типа молчит там, где прежняя регулярка находку давала.
        let facts = collect_facts("Р = Запрос.Выполнть().Выбрать();");
        assert!(
            facts
                .dots
                .iter()
                .any(|d| d.head == "Запрос" && d.member == "Выполнть"),
            "нет звена Запрос.Выполнть: {:?}",
            facts.dots.iter().map(|d| (&d.head, &d.member)).collect::<Vec<_>>()
        );
        // Голова цепочки не должна попасть в голые вызовы.
        assert!(facts.calls.is_empty(), "цепочка дала голый вызов: {:?}",
            facts.calls.iter().map(|c| &c.name).collect::<Vec<_>>());
    }

    #[test]
    fn simple_member_call_is_not_duplicated() {
        // Один DotFact, а не два: `call_expression` и вложенный `access`
        // не должны собрать одно и то же звено дважды.
        let facts = collect_facts("Р = Запрос.Выполнить();");
        let links: Vec<(&str, &str)> = facts
            .dots
            .iter()
            .map(|d| (d.head.as_str(), d.member.as_str()))
            .collect();
        assert_eq!(links, vec![("Запрос", "Выполнить")]);
    }

    #[test]
    fn normalize_keeps_byte_length() {
        // Позиции узлов совпадают с оригиналом только если длина не изменилась.
        for src in [
            "Х = ? (Усл, 1, 2);",
            "Попытка\n А();\nИсключение\n Если Б Тогда\n  ВызватьИсключение;\n КонецЕсли;\nКонецПопытки;",
            "Процедура СчётаУчёта()\nКонецПроцедуры",
            "Х = ?(Усл, 1, 2);", // трогать нечего
        ] {
            let n = normalize_for_parser(src);
            assert_eq!(n.len(), src.len(), "длина изменилась для: {src}");
        }
    }

    #[test]
    fn ternary_with_space_is_parsed() {
        // `? (Усл, ...)` в грамматике 0.1.7 — ERROR; после нормализации разбирается,
        // и вложенный вызов внутри тернарного оператора становится виден.
        let facts = collect_facts("Кол = ? (Стр.Свойство(\"К\"), СтрокаЧисло(Стр.К), 0);");
        let names: Vec<&str> = facts.calls.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"СтрокаЧисло"), "вызов внутри тернарного не найден: {names:?}");
        assert!(facts.dots.iter().any(|d| d.head == "Стр" && d.member == "Свойство"));
    }

    #[test]
    fn bare_raise_does_not_break_tree() {
        // `ВызватьИсключение;` внутри `Если` грамматика не понимает.
        let facts = collect_facts(
            "Процедура П()\n Попытка\n  А();\n Исключение\n  Если Б Тогда\n   ВызватьИсключение;\n  КонецЕсли;\n  Лог(В);\n КонецПопытки;\nКонецПроцедуры",
        );
        assert!(facts.declarations.contains("п"));
        let names: Vec<&str> = facts.calls.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"Лог"), "вызов после ВызватьИсключение потерян: {names:?}");
    }

    #[test]
    fn raise_with_argument_is_untouched() {
        // Форму с аргументом грамматика понимает — нормализация её не трогает.
        let src = "Попытка\n А();\nИсключение\n ВызватьИсключение \"текст\";\nКонецПопытки;";
        assert!(matches!(normalize_for_parser(src), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn nbsp_indent_is_parsed() {
        // Неразрывный пробел (U+00A0) в отступе грамматика пробелом не считает.
        let src = "Процедура П()\n\u{00A0}\u{00A0}Сообщить(1);\n  Лог(2);\nКонецПроцедуры";
        assert_eq!(normalize_for_parser(src).len(), src.len());
        let facts = collect_facts(src);
        let names: Vec<&str> = facts.calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["Сообщить", "Лог"]);
        assert!(facts.declarations.contains("п"));
    }

    #[test]
    fn preproc_with_space_is_parsed() {
        // `# Если` с пробелом после решётки — ERROR, `#Если` — нет.
        let src = "Процедура П()\n  Лог(1);\n# Если Клиент Тогда\n  Сообщить(2);\n#КонецЕсли\nКонецПроцедуры";
        assert_eq!(normalize_for_parser(src).len(), src.len());
        let facts = collect_facts(src);
        let names: Vec<&str> = facts.calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["Лог", "Сообщить"]);
    }

    #[test]
    fn method_of_ternary_result_is_not_a_bare_call() {
        // `?(У, А, Б).ПолучитьИмена()` грамматика рвёт так, что вызов метода
        // становится отдельным оператором. Точка слева спасает от ложной находки.
        let facts = collect_facts("Процедура П()\n  А = ?(У, Б, В).ПолучитьИмена();\n  Лог(3);\nКонецПроцедуры");
        let names: Vec<&str> = facts.calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["Лог"], "метод результата тернарного принят за голый вызов: {names:?}");
    }

    #[test]
    fn constructor_survives_preprocessor_inside_arguments() {
        // Директива `#Если` внутри списка аргументов грамматике неизвестна: узел
        // `new_expression` разваливается, и `Новый ОписаниеТипов(...)` приходит
        // обычным вызовом. Слово `Новый` слева спасает от ложной находки.
        // Взято из `external/Выгрузка накладных в Docsinbox` (35 ложных находок на УТ).
        let src = "Процедура П()\n\
                   \x20   В = Новый Структура(\"а,б\",\n\
                   \x20       Новый ОписаниеТипов(\"Строка\"),\n\
                   #Если ВебКлиент Тогда\n\
                   \x20       Новый ОписаниеТипов(\"Массив\"),\n\
                   #КонецЕсли\n\
                   \x20   );\n\
                   КонецПроцедуры";
        let facts = collect_facts(src);
        let calls: Vec<&str> = facts.calls.iter().map(|c| c.name.as_str()).collect();
        assert!(calls.is_empty(), "конструктор принят за голый вызов: {calls:?}");
        assert!(facts.news.iter().any(|n| n.type_name == "ОписаниеТипов"));
    }

    #[test]
    fn new_keyword_check_is_case_insensitive_and_word_bounded() {
        // `Обновый` — не `Новый`; регистр значения не имеет.
        let facts = collect_facts("Процедура П()\n  А = НОВЫЙ Массив();\n  Б = Обновый(1);\nКонецПроцедуры");
        let calls: Vec<&str> = facts.calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(calls, vec!["Обновый"], "ожидался только вызов Обновый: {calls:?}");
    }

    #[test]
    fn negative_default_keeps_facts() {
        // `Процедура П(А = -1)` — минус в заголовке грамматика не принимает.
        let src = "Процедура П(Знач А = -1, Б = 2)\n  Сообщить(1);\n  Х = Стр.Поле;\nКонецПроцедуры";
        assert_eq!(normalize_for_parser(src).len(), src.len());
        let facts = collect_facts(src);
        assert!(facts.declarations.contains("п"));
        assert_eq!(facts.calls.len(), 1);
        assert_eq!(facts.dots.len(), 1);
    }

    #[test]
    fn negative_value_in_body_is_untouched() {
        // Минус в ТЕЛЕ процедуры грамматике понятен — нормализация его не трогает.
        let src = "Процедура П()\n  Х = -1;\n  У = А - Б;\nКонецПроцедуры";
        assert!(matches!(normalize_for_parser(src), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn chain_head_call_is_bare() {
        // `ПустаяСсылка().Метаданные()`: слева от головы точки нет — это голый
        // вызов. В дереве он лежит внутри `access` единственным ребёнком.
        let facts = collect_facts("Х = ПустаяСсылка().Метаданные().ПолноеИмя();");
        let names: Vec<&str> = facts.calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["ПустаяСсылка"], "голова цепочки должна быть голым вызовом");
    }

    #[test]
    fn member_call_in_chain_is_not_bare() {
        // А `Запрос.Выполнить()` — метод объекта, голым вызовом быть не должен.
        let facts = collect_facts("Р = Запрос.Выполнить().Выбрать();");
        assert!(facts.calls.is_empty(), "лишние голые вызовы: {:?}",
            facts.calls.iter().map(|c| &c.name).collect::<Vec<_>>());
    }

    #[test]
    fn yo_letter_does_not_break_identifiers() {
        // Грамматика не считает `ё` частью идентификатора (диапазон `а-я` её не
        // включает). Без нормализации `СчётаУчёта` рвётся на «Сч»/«таУч»/«та»,
        // объявление теряется, а обрубки становятся ложными находками.
        let facts = collect_facts(
            "Процедура СчётаУчёта()\n    ЗаполнённыеДанные();\nКонецПроцедуры\n\
             Процедура ЗаполнённыеДанные()\nКонецПроцедуры\n",
        );
        assert_eq!(
            facts.declarations,
            HashSet::from(["счётаучёта".to_string(), "заполнённыеданные".to_string()]),
            "объявления с ё: {:?}",
            facts.declarations
        );
        let names: Vec<&str> = facts.calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["ЗаполнённыеДанные"], "имя вызова должно сохранить ё");
    }

    #[test]
    fn yo_letter_in_member_name() {
        let facts = collect_facts("Х = ЦветаСтиля.УОП_ЗелёнаяСтрока;");
        assert_eq!(facts.dots.len(), 1);
        assert_eq!(facts.dots[0].head, "ЦветаСтиля");
        assert_eq!(facts.dots[0].member, "УОП_ЗелёнаяСтрока");
    }

    #[test]
    fn declarations_and_call_inside_module() {
        let facts = collect_facts(
            "Процедура А()\n  Б();\nКонецПроцедуры\nФункция Б()\n  Возврат 1;\nКонецФункции",
        );
        assert_eq!(
            facts.declarations,
            HashSet::from(["а".to_string(), "б".to_string()])
        );
        let names: Vec<&str> = facts.calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["Б"]);
    }

    #[test]
    fn mask_keeps_positions() {
        let src = "Если А = \"строка\" Тогда";
        let masked = mask_strings_and_comments(src);
        assert_eq!(masked.len(), src.len());
        // Текст вне строки сохранился.
        assert!(masked.contains("Если А ="));
        // Содержимое строки замаскировано.
        assert!(!masked.contains("строка"));
    }

    #[test]
    fn mask_handles_comment_to_eol() {
        let src = "А = 1; // комментарий\nБ = 2;";
        let masked = mask_strings_and_comments(src);
        assert!(masked.contains("А = 1;"));
        assert!(!masked.contains("комментарий"));
        assert!(masked.contains("Б = 2;"));
    }

    #[test]
    fn collect_methods_directive_and_export() {
        let methods = collect_methods("&НаСервере\nПроцедура Тест(А, Знач Б = 1) Экспорт\nКонецПроцедуры");
        assert_eq!(methods.len(), 1);
        let m = &methods[0];
        assert_eq!(m.name, "Тест");
        assert!(!m.is_function);
        assert!(m.is_export);
        assert_eq!(m.directive.as_deref(), Some("НаСервере"));
        assert_eq!(m.params.as_deref(), Some("(А, Знач Б = 1)"));
        assert_eq!(m.line_start, 2);
    }

    #[test]
    fn collect_methods_function_export() {
        let methods = collect_methods("Функция Ф() Экспорт\n  Возврат 1;\nКонецФункции");
        assert_eq!(methods.len(), 1);
        assert!(methods[0].is_function);
        assert!(methods[0].is_export);
    }

    #[test]
    fn collect_methods_normalizes_yo() {
        let methods = collect_methods("Процедура СчётаУчёта()\nКонецПроцедуры");
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].name, "СчётаУчёта");
    }

    #[test]
    fn collect_methods_override_directive() {
        let methods = collect_methods("&Вместо(\"Ф\")\nПроцедура Р()\nКонецПроцедуры");
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].directive.as_deref(), Some("Вместо"));
    }
}
