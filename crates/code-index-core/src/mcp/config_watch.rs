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

use std::collections::{BTreeMap, BTreeSet};
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
use crate::federation::reload::{absolute_path, ServeConfigReloader};
use crate::watcher::is_config_change;

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
    let daemon_toml_path = absolute_path(&daemon_toml_path)?;
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
    let _debouncer = build_debouncer(vec![daemon_toml_path.clone()], tx)?;

    tracing::info!(
        "config_watch: отслеживаю изменения {} (debounce 500мс)",
        daemon_toml_path.display()
    );

    // Первичный сбор языков здесь НЕ делается: он выполняется вызывающей
    // стороной синхронно, до запуска транспорта (`apply_languages_from_config`).
    // Пока он жил тут, получалась гонка — задача наблюдателя и обслуживание
    // клиента стартовали одновременно, и клиент, спросивший перечень
    // инструментов первым, получал набор без инструментов языка.

    while let Some(event) = rx.recv().await {
        match event {
            Ok(events) => {
                // Любая операция на файле = повод перечитать. Не разбираем
                // конкретный kind — на Windows write часто приходит как
                // Modify(Any), на Linux может быть и Create при atomic-rename.
                if !events.is_empty() {
                    if let Err(e) = reload_from_disk(&server, &daemon_toml_path).await {
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

/// Запустить единый watcher пары федеративных конфигов.
pub fn spawn_federated_watch(reloader: ServeConfigReloader) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run_federated_watch(reloader).await {
            tracing::error!("config_watch федерации завершился с ошибкой: {}", e);
        }
    })
}

async fn run_federated_watch(reloader: ServeConfigReloader) -> Result<()> {
    let targets = vec![
        reloader.serve_path().to_path_buf(),
        reloader.daemon_path().to_path_buf(),
    ];
    let (tx, mut rx) = mpsc::channel::<DebounceEventResult>(16);
    let _debouncer = build_debouncer(targets.clone(), tx)?;
    tracing::info!(
        "config_watch: отслеживаю изменения {} и {} (debounce 500мс)",
        targets[0].display(),
        targets[1].display()
    );
    while let Some(event) = rx.recv().await {
        match event {
            Ok(events) if !events.is_empty() => {
                reloader.reload().await;
            }
            Ok(_) => {}
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
    targets: Vec<PathBuf>,
    tx: mpsc::Sender<DebounceEventResult>,
) -> Result<Debouncer<RecommendedWatcher, RecommendedCache>> {
    let parents: BTreeSet<PathBuf> = targets
        .iter()
        .map(|target| {
            target.parent().map(Path::to_path_buf).ok_or_else(|| {
                anyhow::anyhow!(
                    "config_watch: у пути {} нет parent — не на что подписываться",
                    target.display()
                )
            })
        })
        .collect::<Result<_>>()?;

    let mut debouncer = new_debouncer(
        Duration::from_millis(500),
        None,
        move |res: DebounceEventResult| {
            // Фильтруем события — нам интересен только сам daemon.toml.
            // Debouncer прокидывает события на всю директорию, но мы
            // сидим только на daemon_toml_path.
            //
            // События доступа (открытие/закрытие на чтение) отбрасываем:
            // на Linux inotify подписан в том числе на IN_OPEN и
            // IN_CLOSE_NOWRITE, поэтому наше же чтение конфига в
            // `reload_from_disk` порождало событие, которое запускало
            // следующее чтение — бесконечная петля с периодом debounce
            // (на ВМ rag: ~11800 перечитываний за два часа). На Windows
            // не воспроизводилось — тамошний watcher о чтении не сообщает.
            let filtered: DebounceEventResult = match res {
                Ok(events) => Ok(events
                    .into_iter()
                    .filter(|e| is_config_change(&e.kind, &e.paths, &targets))
                    .collect()),
                Err(errors) => Err(errors),
            };
            // Игнорируем ошибки `try_send` — если канал переполнен,
            // следующее событие всё равно перезапустит rebuild.
            let _ = tx.blocking_send(filtered);
        },
    )?;

    for parent in parents {
        debouncer
            .watch(parent.as_path(), RecursiveMode::NonRecursive)
            .map_err(|e| {
                anyhow::anyhow!(
                    "config_watch: не удалось watch '{}': {}",
                    parent.display(),
                    e
                )
            })?;
    }
    Ok(debouncer)
}

/// Собрать активные языки из конфигурации и применить их к серверу.
///
/// Вызывается дважды: один раз синхронно при старте сервера (до того как
/// клиент сможет спросить перечень инструментов) и затем на каждое изменение
/// файла настроек. Идемпотентна.
pub async fn apply_languages_from_config(
    server: &CodeIndexServer,
    daemon_toml_path: &Path,
) -> Result<()> {
    reload_from_disk(server, daemon_toml_path).await
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

    let languages = path_languages(&cfg);
    // Язык и процессор записей — до подмены инструментов: инструмент языка,
    // вызванный сразу после перечитки, должен застать привязанный процессор.
    server.apply_repo_languages(&languages);
    let active: BTreeSet<String> = languages.into_values().collect();

    tracing::info!(
        "config_watch: перечитан {}, активные языки: {:?}",
        daemon_toml_path.display(),
        active.iter().collect::<Vec<_>>()
    );
    server.reload_extensions(active).await;
    Ok(())
}

/// Язык каждой записи `[[paths]]` по её алиасу (`effective_alias`): явный
/// `language` или автоопределение по корню — тем же способом, каким поле
/// заполняет демон. Записи, язык которых не определился, в карту не входят.
pub(crate) fn path_languages(cfg: &config::DaemonFileConfig) -> BTreeMap<String, String> {
    // Собираем язык каждой записи. У записи без `language` язык
    // определяем сами — тем же способом, каким его заполняет демон при
    // старте. Раньше такие записи просто пропускались, и получалось так:
    // конфигурация 1С подключена, а одиннадцать инструментов 1С в перечне
    // отсутствуют, потому что демон на этом файле ещё не отработал и поле
    // не дописал. Со стороны это выглядит как «инструментов нет вовсе».
    let mut languages = BTreeMap::new();
    for entry in &cfg.paths {
        match &entry.language {
            Some(lang) => {
                languages.insert(entry.effective_alias(), lang.clone());
            }
            None => {
                let root = entry
                    .path
                    .canonicalize()
                    .unwrap_or_else(|_| entry.path.clone());
                if let Some(lang) = crate::daemon_core::language_detect::detect_language(&root) {
                    tracing::info!(
                        "config_watch: язык для {} не задан, определён автоматически: {}",
                        entry.path.display(),
                        lang
                    );
                    languages.insert(entry.effective_alias(), lang.to_string());
                }
            }
        }
    }

    languages
}

/// Множество активных языков для федеративного перечитывателя — те же
/// языки, что у записей в [`path_languages`].
pub(crate) fn active_languages(cfg: &config::DaemonFileConfig) -> BTreeSet<String> {
    path_languages(cfg).into_values().collect()
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
            processor: None,
        }
    }

    /// Запись без `language` раньше просто пропускалась при сборе активных
    /// языков: пока демон не отработал на этом файле и не дописал поле,
    /// инструменты языка не появлялись в перечне вовсе. Теперь язык
    /// определяется тем же автоопределением, что и у демона.
    #[tokio::test]
    async fn язык_без_явной_настройки_определяется_автоматически() {
        let tmp = TempDir::new().unwrap();

        // «Репозиторий» с признаком языка в корне.
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::File::create(repo.join("Configuration.xml")).unwrap();

        // Конфигурация без `language` — как её пишет человек руками.
        let cfg_path = tmp.path().join("daemon.toml");
        let mut f = std::fs::File::create(&cfg_path).unwrap();
        write!(
            f,
            "[[paths]]\npath = '{}'\n",
            repo.canonicalize().unwrap().display()
        )
        .unwrap();
        drop(f);

        let mut repos = BTreeMap::new();
        repos.insert("stand".to_string(), dummy_repo());
        let mut registry = ProcessorRegistry::new();
        registry.register(StdArc::new(FakeBslProcessor));
        let server = CodeIndexServer::with_repos_and_registry(repos, registry);

        reload_from_disk(&server, &cfg_path).await.unwrap();

        assert!(
            server.active_language_names().contains(&"bsl".to_string()),
            "язык должен определиться по признаку в корне: {:?}",
            server.active_language_names()
        );
        assert_eq!(
            server.extension_tools_count(),
            1,
            "инструменты языка должны появиться и без явной настройки"
        );
    }

    /// Перечитка `daemon.toml` проставляет язык И процессор записи, у которой
    /// `language` не задан явно, — иначе инструменты языка, взятые из
    /// перечня, работали бы без привязок. Remote-записи не трогаются.
    #[tokio::test]
    async fn перечитка_проставляет_язык_и_процессор_локальной_записи() {
        let tmp = TempDir::new().unwrap();

        // «Репозиторий» с признаком языка в корне.
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::File::create(repo.join("Configuration.xml")).unwrap();

        // Конфигурация без `language` — язык должен определиться сам.
        let cfg_path = tmp.path().join("daemon.toml");
        let mut f = std::fs::File::create(&cfg_path).unwrap();
        write!(
            f,
            "[[paths]]\npath = '{}'\nalias = 'stand'\n",
            repo.canonicalize().unwrap().display()
        )
        .unwrap();
        drop(f);

        let mut repos = BTreeMap::new();
        repos.insert(
            "stand".to_string(),
            RepoEntry {
                root_path: Some(repo),
                storage: None,
                ip: LEGACY_OWN_IP.to_string(),
                port: crate::federation::client::DEFAULT_REMOTE_PORT,
                is_local: true,
                language: None,
                processor: None,
            },
        );
        repos.insert("remote".to_string(), dummy_repo());
        let mut registry = ProcessorRegistry::new();
        registry.register(StdArc::new(FakeBslProcessor));
        let server = CodeIndexServer::with_repos_and_registry(repos, registry);

        assert!(
            server
                .repos
                .load()
                .get("stand")
                .unwrap()
                .processor
                .is_none(),
            "до перечитки процессор ещё не привязан"
        );

        reload_from_disk(&server, &cfg_path).await.unwrap();

        let stand = server.repos.load().get("stand").cloned().unwrap();
        assert_eq!(stand.language, Some("bsl".to_string()));
        assert_eq!(stand.processor.as_ref().map(|p| p.name()), Some("bsl"));

        let remote = server.repos.load().get("remote").cloned().unwrap();
        assert!(remote.language.is_none());
        assert!(remote.processor.is_none());
    }

    /// Событие «файл открыли/закрыли на чтение» не должно считаться
    /// изменением конфига: наше же чтение в `reload_from_disk` порождает
    /// такое событие (Linux inotify подписан на IN_OPEN/IN_CLOSE_NOWRITE),
    /// и без фильтра получалась бесконечная петля перечитывания.
    #[test]
    fn access_events_do_not_trigger_reload() {
        use notify_debouncer_full::notify::event::{
            AccessKind, AccessMode, CreateKind, ModifyKind,
        };
        use notify_debouncer_full::notify::EventKind;

        let target = PathBuf::from("/cfg/daemon.toml");
        let paths = vec![target.clone()];

        assert!(!is_config_change(
            &EventKind::Access(AccessKind::Open(AccessMode::Read)),
            &paths,
            std::slice::from_ref(&target)
        ));
        assert!(!is_config_change(
            &EventKind::Access(AccessKind::Close(AccessMode::Read)),
            &paths,
            std::slice::from_ref(&target)
        ));
        // Реальная правка — по-прежнему повод перечитать.
        assert!(is_config_change(
            &EventKind::Modify(ModifyKind::Any),
            &paths,
            std::slice::from_ref(&target)
        ));
        assert!(is_config_change(
            &EventKind::Create(CreateKind::File),
            &paths,
            std::slice::from_ref(&target)
        ));
        // Событие по соседнему файлу каталога — не наше дело.
        assert!(!is_config_change(
            &EventKind::Modify(ModifyKind::Any),
            &[PathBuf::from("/cfg/daemon.json")],
            std::slice::from_ref(&target)
        ));
    }

    #[test]
    fn federated_filter_accepts_both_targets_only() {
        use notify_debouncer_full::notify::event::{AccessKind, AccessMode, ModifyKind};
        use notify_debouncer_full::notify::EventKind;

        let serve = PathBuf::from("/cfg/serve.toml");
        let daemon = PathBuf::from("/cfg/daemon.toml");
        let targets = vec![serve.clone(), daemon.clone()];
        let modified = EventKind::Modify(ModifyKind::Any);

        assert!(is_config_change(&modified, &[serve], &targets));
        assert!(is_config_change(&modified, &[daemon], &targets));
        assert!(!is_config_change(
            &modified,
            &[PathBuf::from("/cfg/neighbor.toml")],
            &targets
        ));
        assert!(!is_config_change(
            &EventKind::Access(AccessKind::Open(AccessMode::Read)),
            &[targets[0].clone()],
            &targets
        ));
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
