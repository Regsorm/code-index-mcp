// File-watch на `daemon.toml` со стороны MCP-сервера.
//
// Когда `code-index serve --config <daemon.toml>` запущен в HTTP-режиме,
// этот модуль поднимает фоновый task, подписывающийся на изменения файла
// конфига через `notify`. На каждое событие изменения task:
//
//  1. Читает обновлённый `daemon.toml` через `daemon_core::config::load_from`.
//  2. Собирает множество активных языков по `[[paths]].language`.
//  3. Зовёт `server.reload_extensions(set)` — это атомарно подменяет
//     `extension_tools` и (если состав языков изменился) шлёт клиенту
//     `notifications/tools/list_changed`.
//
// Демону этот модуль не нужен — у него свой watcher на исходники, а
// `daemon.toml` ему перечитывается через `daemon reload` или future
// этапа 1.8 (auto-detect при старте). MCP-сервер же без notify зависел
// бы от ручного рестарта, поэтому именно тут file-watch критичен.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use notify_debouncer_full::{
    new_debouncer,
    notify::{RecommendedWatcher, RecursiveMode},
    DebounceEventResult, Debouncer, RecommendedCache,
};
use tokio::sync::mpsc;

use super::CodeIndexServer;
use crate::daemon_core::config;

/// Запускает background task, отслеживающий изменения `daemon.toml`.
/// Возвращает `JoinHandle`, по которому caller может дождаться завершения
/// (на практике — никогда, watcher живёт пока живёт сервер) или просто
/// бросить (`drop` отвяжет, ничего не сломается).
///
/// Debounce — 500мс: редакторы часто пишут файл несколькими операциями
/// (truncate → write → rename), без debounce мы реактивим N раз подряд.
pub fn spawn_watch(
    server: CodeIndexServer,
    daemon_toml_path: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run_watch(server, daemon_toml_path).await {
            tracing::error!("config_watch завершился с ошибкой: {}", e);
        }
    })
}

/// Внутренний event-loop. Вынесен отдельной функцией, чтобы `?` ловило
/// ошибки в одном месте и task мог их залогировать.
async fn run_watch(server: CodeIndexServer, daemon_toml_path: PathBuf) -> Result<()> {
    if !daemon_toml_path.exists() {
        tracing::warn!(
            "config_watch: {} не существует на момент старта watcher'а; \
             watch включится при создании файла.",
            daemon_toml_path.display()
        );
    }

    // notify не работает напрямую с tokio. Идиома: создать sync-канал
    // (mpsc), debouncer пишет в него из своего thread-pool, наш async-task
    // читает через `recv().await`.
    let (tx, mut rx) = mpsc::channel::<DebounceEventResult>(16);
    let _debouncer = build_debouncer(&daemon_toml_path, tx)?;

    tracing::info!(
        "config_watch: отслеживаю изменения {} (debounce 500мс)",
        daemon_toml_path.display()
    );

    // Первичная «затравка» сразу после старта — без неё клиент,
    // подключившийся ДО первого изменения daemon.toml, видел бы
    // только core-tools (extension_tools пуст у server-конструкторов
    // в `cli::run`, потому что RepoEntry.language=None при загрузке
    // из --config). Делаем синхронный rebuild на старте, чтобы
    // tools/list на первом же запросе содержал bsl-tools, если
    // в TOML есть `language = "bsl"`.
    if daemon_toml_path.exists() {
        if let Err(e) = reload_from_disk(&server, &daemon_toml_path).await {
            tracing::warn!(
                "config_watch: первичная инициализация active_languages из {} упала: {}",
                daemon_toml_path.display(),
                e
            );
        }
    }

    while let Some(event) = rx.recv().await {
        match event {
            Ok(events) => {
                // Любая операция на файле = повод перечитать. Не разбираем
                // конкретный kind — на Windows write часто приходит как
                // Modify(Any), на Linux может быть и Create при atomic-rename.
                if !events.is_empty() {
                    if let Err(e) =
                        reload_from_disk(&server, &daemon_toml_path).await
                    {
                        tracing::warn!(
                            "config_watch: не удалось применить изменения {}: {}",
                            daemon_toml_path.display(),
                            e
                        );
                    }
                }
            }
            Err(errors) => {
                for err in errors {
                    tracing::warn!("config_watch: notify error: {}", err);
                }
            }
        }
    }
    Ok(())
}

/// Собрать `Debouncer` и подписать его на родительскую директорию
/// `daemon.toml`. Подписываемся на директорию, а не на сам файл, потому
/// что atomic-rename редактора (написать в .tmp → rename) удаляет
/// inode исходного файла и watch на нём перестаёт срабатывать.
fn build_debouncer(
    daemon_toml_path: &Path,
    tx: mpsc::Sender<DebounceEventResult>,
) -> Result<Debouncer<RecommendedWatcher, RecommendedCache>> {
    let parent = daemon_toml_path
        .parent()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "config_watch: у пути {} нет parent — не на что подписываться",
                daemon_toml_path.display()
            )
        })?
        .to_path_buf();
    let target = daemon_toml_path.to_path_buf();

    let mut debouncer = new_debouncer(
        Duration::from_millis(500),
        None,
        move |res: DebounceEventResult| {
            // Фильтруем события — нам интересен только сам daemon.toml.
            // Debouncer прокидывает события на всю директорию, но мы
            // сидим только на daemon_toml_path.
            let filtered: DebounceEventResult = match res {
                Ok(events) => Ok(events
                    .into_iter()
                    .filter(|e| e.paths.iter().any(|p| p == &target))
                    .collect()),
                Err(errors) => Err(errors),
            };
            // Игнорируем ошибки `try_send` — если канал переполнен,
            // следующее событие всё равно перезапустит rebuild.
            let _ = tx.blocking_send(filtered);
        },
    )?;

    debouncer
        .watch(parent.as_path(), RecursiveMode::NonRecursive)
        .map_err(|e| anyhow::anyhow!("config_watch: не удалось watch '{}': {}", parent.display(), e))?;
    Ok(debouncer)
}

/// Перечитать конфиг и пересобрать active_languages. Дальше — в сервер.
async fn reload_from_disk(server: &CodeIndexServer, daemon_toml_path: &Path) -> Result<()> {
    if !daemon_toml_path.exists() {
        // Файл удалён — оставляем сервер в текущем состоянии,
        // не делаем «всё пусто». Это разумно: оператор скорее всего
        // редактирует через atomic-rename, файл вернётся через миг.
        tracing::warn!(
            "config_watch: {} временно отсутствует, пропускаю rebuild",
            daemon_toml_path.display()
        );
        return Ok(());
    }
    let cfg = config::load_from(daemon_toml_path)?;

    // Собираем множество активных языков. Записи без `language` —
    // пропускаются (на этапе 1.8 auto-detect заполнит их и сделает
    // отдельный `daemon reload` / следующее событие watcher'а).
    let mut active = BTreeSet::new();
    for entry in &cfg.paths {
        if let Some(lang) = &entry.language {
            active.insert(lang.clone());
        }
    }

    tracing::info!(
        "config_watch: перечитан {}, активные языки: {:?}",
        daemon_toml_path.display(),
        active.iter().collect::<Vec<_>>()
    );
    server.reload_extensions(active).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::sync::Arc as StdArc;
    use tempfile::TempDir;

    use crate::extension::{IndexTool, LanguageProcessor, ProcessorRegistry, ToolContext};
    use crate::mcp::{CodeIndexServer, RepoEntry, LEGACY_OWN_IP};

    /// Минимальный фейк для тестов — повторяет структуру из mcp::tests
    /// (отдельный, чтобы не тянуть зависимости между тестами).
    struct FakeBslTool;
    impl IndexTool for FakeBslTool {
        fn name(&self) -> &str {
            "fake_bsl_tool"
        }
        fn description(&self) -> &str {
            "test"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn execute<'a>(
            &'a self,
            _args: serde_json::Value,
            _ctx: ToolContext<'a>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = serde_json::Value> + Send + 'a>>
        {
            Box::pin(async { serde_json::json!({}) })
        }
    }
    struct FakeBslProcessor;
    impl LanguageProcessor for FakeBslProcessor {
        fn name(&self) -> &str {
            "bsl"
        }
        fn additional_tools(&self) -> Vec<StdArc<dyn IndexTool>> {
            vec![StdArc::new(FakeBslTool)]
        }
    }

    fn dummy_repo() -> RepoEntry {
        RepoEntry {
            root_path: None,
            storage: None,
            ip: LEGACY_OWN_IP.to_string(),
            port: crate::federation::client::DEFAULT_REMOTE_PORT,
            is_local: false,
            language: None,
        }
    }

    /// Прямой вызов `reload_from_disk` с подсунутым daemon.toml —
    /// проверяем что после правки файла активные языки и tools
    /// обновляются. Сам watcher в тесте не запускаем (он требует
    /// настоящих filesystem-событий и flaky на CI), достаточно
    /// проверить логику reload_from_disk.
    #[tokio::test]
    async fn reload_from_disk_picks_up_languages_from_toml() {
        let tmp = TempDir::new().unwrap();
        let toml_path = tmp.path().join("daemon.toml");

        std::fs::File::create(&toml_path)
            .unwrap()
            .write_all(
                br#"
[[paths]]
path = "/tmp/x"
language = "bsl"

[[paths]]
path = "/tmp/y"
language = "python"

[[paths]]
path = "/tmp/z"
"#,
            )
            .unwrap();

        // Сервер с одним python-репо (без bsl) и реестром, содержащим
        // BSL-процессор. Изначально extension_tools пуст.
        let mut repos = BTreeMap::new();
        repos.insert("py".to_string(), dummy_repo());
        let mut reg = ProcessorRegistry::new();
        reg.register(StdArc::new(FakeBslProcessor));
        let server = CodeIndexServer::with_repos_and_registry(repos, reg);
        assert_eq!(server.extension_tools_count(), 0);

        // Имитируем file-watch'а: вызываем reload_from_disk напрямую.
        reload_from_disk(&server, &toml_path).await.unwrap();

        // bsl активирован — fake_bsl_tool должен попасть в extension_tools.
        let names = server.active_language_names();
        assert!(names.contains(&"bsl".to_string()));
        assert!(names.contains(&"python".to_string()));
        assert_eq!(server.extension_tools_count(), 1);
    }

    /// Если файл временно исчез — reload_from_disk не должен зануливать
    /// state (atomic rename редактора восстановит файл через миг).
    #[tokio::test]
    async fn reload_from_disk_keeps_state_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let toml_path = tmp.path().join("never_existed.toml");

        let server = CodeIndexServer::with_repos(BTreeMap::new());
        // Не паникуем, не валимся в Err.
        reload_from_disk(&server, &toml_path).await.unwrap();
        assert_eq!(server.extension_tools_count(), 0);
    }
}
