// Trait `IndexTool` — описание одного MCP-инструмента, добавляемого
// расширением (например, BSL-специфичный `get_object_structure`).
//
// Почему отдельный trait, а не просто rmcp `#[tool]`-метод на сервере:
// rmcp-роутер знает про tools на этапе компиляции (макрос `#[tool_router]`
// инстанцируется по списку методов), а нам нужно динамически собрать
// набор tools из активных `LanguageProcessor`-ов. Поэтому tool-методы
// core остаются на rmcp-роутере (они есть всегда), а tools от расширений
// регистрируются через этот trait и добавляются в ответ `tools/list`
// рантайм-кодом.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use super::context::ToolContext;

/// Описание одного MCP-инструмента, поставляемого расширением.
///
/// Реализации должны быть `Send + Sync` — один экземпляр шарится
/// между всеми сессиями MCP-сервера.
pub trait IndexTool: Send + Sync {
    /// Имя инструмента в MCP-протоколе. Должно быть стабильным —
    /// клиенты привязываются к нему.
    fn name(&self) -> &str;

    /// Человекочитаемое описание (попадает в `tools/list` и видно
    /// LLM-у при выборе инструмента).
    fn description(&self) -> &str;

    /// JSON-Schema параметров. Возвращается как `serde_json::Value`,
    /// чтобы расширения могли строить схему через `schemars` или
    /// руками — без принудительной привязки к конкретной библиотеке.
    fn input_schema(&self) -> Value;

    /// Какие языки поддерживает инструмент.
    /// `None` — универсальный (применим к любому репо).
    /// `Some(&["bsl"])` — только для BSL-репо.
    ///
    /// Используется и для conditional registration (tool попадает в
    /// `tools/list` если хотя бы один активный язык совместим), и
    /// для runtime-валидации в `execute` (защита от ошибок диспатча).
    fn applicable_languages(&self) -> Option<&'static [&'static str]> {
        None
    }

    /// Выполнить инструмент. Возвращает JSON-ответ — тот же формат,
    /// что отдают core-tools (см. `mcp::tools::*`).
    ///
    /// Возвращает `Pin<Box<dyn Future>>` чтобы trait был object-safe
    /// (нельзя `async fn` в trait без `async-trait` macro). Обвязка
    /// громоздкая, но это меньшее зло чем тащить лишнюю зависимость.
    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Value> + Send + 'a>>;
}
