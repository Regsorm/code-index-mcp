// Git-инструменты MCP-слоя: история, изменения и содержимое файлов по
// ревизиям. Слой read-only: используются только `rev-parse`, `log`, `diff`,
// `show` и `rev-list -1 --before=<момент>`; ни одна команда не пишет в
// репозиторий и не ходит в сеть.
//
// Координаты путей. Входные пути инструментов — буквальные пути относительно
// корня индекса (`PathEntry.path`). Для рабочего дерева git-команды
// выполняются из индексного корня с `--relative`, поэтому и выдача остаётся в
// индексных координатах, а изменения вне подкаталога не видны. Для bare
// репозитория (`--git-dir`) `--relative` неприменим: пути и там, и в ответах
// считаются от корня репозитория — это единственное исключение.

// Ошибка инструмента сериализуется целиком как ответ клиенту, поэтому
// диагностические поля лежат рядом, а не в Box: разворачивать их при каждом
// `?` было бы дороже, чем 200 байт в Err-ветке.
#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{Local, LocalResult, NaiveDate, NaiveDateTime, TimeZone};
use serde::Serialize;

use super::RepoEntry;

/// Категории выгрузки 1С: папка (`object_name` в `metadata_modules` —
/// множественное имя) → тип метаданных для `meta_type`. Всё остальное
/// (`External`, неизвестные каталоги) даёт `meta_type: null`.
const KNOWN_CATEGORIES: [(&str, &str); 30] = [
    ("AccumulationRegisters", "AccumulationRegister"),
    ("AccountingRegisters", "AccountingRegister"),
    ("BusinessProcesses", "BusinessProcess"),
    ("CalculationRegisters", "CalculationRegister"),
    ("Catalogs", "Catalog"),
    ("ChartsOfAccounts", "ChartOfAccounts"),
    ("ChartsOfCalculationTypes", "ChartOfCalculationTypes"),
    ("ChartsOfCharacteristicTypes", "ChartOfCharacteristicTypes"),
    ("CommandGroups", "CommandGroup"),
    ("CommonAttributes", "CommonAttribute"),
    ("CommonCommands", "CommonCommand"),
    ("CommonForms", "CommonForm"),
    ("CommonModules", "CommonModule"),
    ("CommonPictures", "CommonPicture"),
    ("CommonTemplates", "CommonTemplate"),
    ("Constants", "Constant"),
    ("DataProcessors", "DataProcessor"),
    ("DefinedTypes", "DefinedType"),
    ("DocumentJournals", "DocumentJournal"),
    ("Documents", "Document"),
    ("Enums", "Enum"),
    ("EventSubscriptions", "EventSubscription"),
    ("ExchangePlans", "ExchangePlan"),
    ("FilterCriteria", "FilterCriterion"),
    ("FunctionalOptions", "FunctionalOption"),
    ("HTTPServices", "HTTPService"),
    ("InformationRegisters", "InformationRegister"),
    ("Reports", "Report"),
    ("Roles", "Role"),
    ("ScheduledJobs", "ScheduledJob"),
];

/// Тип метаданных по известной категории выгрузки; неизвестная категория —
/// `None` (значение `meta_type` не выдумывается).
fn meta_type_for_category(category: &str) -> Option<String> {
    KNOWN_CATEGORIES
        .iter()
        .find(|(plural, _)| *plural == category)
        .map(|(_, meta_type)| (*meta_type).to_string())
}

/// Сколько путей объекта отдавать в `git_changes_by_object` до отметки
/// обрезки: полный список путей крупного объекта раздувает ответ сильнее,
/// чем перечень объектов.
const PATHS_PER_OBJECT: usize = 50;

/// Размер SQL-пакета для запросов к `metadata_modules`.
const METADATA_CHUNK: usize = 400;

// ── Ошибки ─────────────────────────────────────────────────────────────────

/// Структурированная ошибка git-инструмента.
///
/// Сериализуется как `{error, repo, path?, stderr, …}`: `path` появляется
/// только у ошибок, привязанных к пути, `stderr` — всегда (null, если git не
/// запускался). Диагностические поля (`hint`, `configured_path`,
/// `upper_bound`, `input`) добавляются по необходимости.
#[derive(Debug, Clone, Serialize)]
pub struct GitError {
    pub error: String,
    pub repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configured_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upper_bound: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
}

impl GitError {
    fn new(repo: &str, message: impl Into<String>) -> Self {
        Self {
            error: message.into(),
            repo: repo.to_string(),
            path: None,
            stderr: None,
            hint: None,
            configured_path: None,
            upper_bound: None,
            input: None,
        }
    }

    fn with_path(mut self, path: &str) -> Self {
        self.path = Some(path.to_string());
        self
    }

    fn with_stderr(mut self, stderr: impl Into<String>) -> Self {
        self.stderr = Some(stderr.into());
        self
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    fn with_configured_path(mut self, path: &Path) -> Self {
        self.configured_path = Some(path.display().to_string());
        self
    }

    fn with_input(mut self, input: &str) -> Self {
        self.input = Some(input.to_string());
        self
    }

    fn with_upper_bound(mut self, upper: &str) -> Self {
        self.upper_bound = Some(upper.to_string());
        self
    }

    /// JSON-представление ошибки — то, что получает клиент инструмента.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| "{\"error\":\"не удалось сериализовать ошибку\"}".to_string())
    }
}

// ── Запуск git ─────────────────────────────────────────────────────────────

/// Результат запуска git: код возврата не считается ошибкой сам по себе —
/// вызывающая сторона решает, ожидаем ли он (`rev-parse --verify --quiet`).
struct GitOutput {
    stdout: Vec<u8>,
    stderr: String,
    success: bool,
}

impl GitOutput {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).to_string()
    }

    fn trimmed(&self) -> String {
        self.text().trim().to_string()
    }

    fn stderr_trimmed(&self) -> String {
        self.stderr.trim().to_string()
    }
}

/// Переменные, перенаправляющие git в чужой репозиторий или объектное
/// хранилище. Наследовать их нельзя: serve может быть запущен из оболочки,
/// где они выставлены для другой задачи.
const GIT_ENV_REMOVED: [&str; 12] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_PREFIX",
    "GIT_EXTERNAL_DIFF",
    "GIT_DIFF_OPTS",
];

/// Единственная точка запуска git: без shell, с безопасными опциями и
/// изолированным окружением. Запрещена ленивая сетевая подгрузка объектов.
async fn run_git(cwd: Option<PathBuf>, args: Vec<String>) -> Result<GitOutput, String> {
    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new("git");
        cmd.arg("--no-pager")
            .arg("-c")
            .arg("core.quotepath=false")
            .arg("-c")
            .arg("core.longpaths=true");
        if let Some(dir) = &cwd {
            cmd.arg("-C").arg(dir);
        }
        cmd.args(&args);
        for var in GIT_ENV_REMOVED {
            cmd.env_remove(var);
        }
        cmd.env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            // Ленивая подгрузка объектов из сети запрещена: инструмент обязан
            // работать на локальном клоне, а не тянуть пакеты с origin.
            .env("GIT_NO_LAZY_FETCH", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.output()
    })
    .await
    .map_err(|e| format!("не удалось дождаться git: {e}"))?
    .map_err(|e| format!("не удалось запустить git: {e}"))?;
    Ok(GitOutput {
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        success: output.status.success(),
    })
}

/// Проверка набора аргументов: пользовательский ввод не должен попадать в
/// позицию опции команды. Разрешённые команды — только чтение; `rev-list`
/// допускается исключительно в форме `-1 --before=<момент> <верхняя-граница>`.
fn validate_git_args(args: &[String]) -> Result<(), String> {
    let command = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("");
    match command {
        "rev-parse" | "log" | "diff" | "show" => Ok(()),
        "rev-list" => {
            let shape_ok = args.len() == 4
                && args[0] == "rev-list"
                && args[1] == "-1"
                && args[2].starts_with("--before=")
                && !args[3].starts_with('-');
            if shape_ok {
                Ok(())
            } else {
                Err("rev-list разрешён только как -1 --before=<момент> <верхняя-граница>".into())
            }
        }
        other => Err(format!("команда git '{other}' не разрешена")),
    }
}

// ── Контекст репозитория ───────────────────────────────────────────────────

/// Разобранные координаты git-репозитория для одного MCP-репо.
#[derive(Debug, Clone)]
pub struct GitContext {
    /// Алиас репозитория (для ответов и сообщений).
    pub alias: String,
    /// Корень индекса (`PathEntry.path` в координатах файловой системы).
    pub index_root: PathBuf,
    /// Каталог git (для bare — сам репозиторий).
    pub git_dir: PathBuf,
    /// Корень рабочего дерева; `None` для bare.
    pub work_tree: Option<PathBuf>,
    /// Путь индексного корня от корня рабочего дерева (для bare — пусто).
    pub prefix: PathBuf,
    /// Репозиторий без рабочего дерева.
    pub bare: bool,
}

impl GitContext {
    /// Проверить включение git-слоя и определить координаты репозитория.
    ///
    /// Ошибки содержат точный настроенный путь и stderr git: оператор должен
    /// видеть, какой именно путь проверялся.
    pub async fn discover(alias: &str, entry: &RepoEntry) -> Result<Self, GitError> {
        if !entry.git_config.enabled {
            return Err(GitError::new(
                alias,
                format!("git-слой для репозитория {alias} не включён"),
            )
            .with_hint(
                "добавьте в [[paths]] daemon.toml пару git = true (и при необходимости git_path)",
            ));
        }
        let index_root = entry
            .root_path
            .clone()
            .ok_or_else(|| GitError::new(alias, "у локальной записи нет корневого пути индекса"))?;
        let configured = entry
            .git_config
            .git_path
            .clone()
            .unwrap_or_else(|| index_root.clone());

        let fail = |message: String, stderr: String| {
            GitError::new(alias, message)
                .with_configured_path(&configured)
                .with_stderr(stderr)
        };

        let bare_out = run_git(
            Some(configured.clone()),
            vec!["rev-parse".into(), "--is-bare-repository".into()],
        )
        .await
        .map_err(|e| {
            fail(
                format!("не удалось обнаружить git-репозиторий: {e}"),
                e.clone(),
            )
        })?;
        if !bare_out.success {
            return Err(fail(
                "не удалось обнаружить git-репозиторий по настроенному пути".to_string(),
                bare_out.stderr_trimmed(),
            )
            .with_hint(
                "проверьте git_path: путь должен указывать на рабочий каталог или bare-репозиторий",
            ));
        }
        let bare = bare_out.trimmed() == "true";

        let git_dir = rev_parse_path(&configured, "--absolute-git-dir", alias, &fail).await?;

        let (work_tree, prefix) = if bare {
            (None, PathBuf::new())
        } else {
            let toplevel = rev_parse_path(&configured, "--show-toplevel", alias, &fail).await?;
            let canonical_root =
                std::fs::canonicalize(&index_root).unwrap_or_else(|_| index_root.clone());
            let canonical_top =
                std::fs::canonicalize(&toplevel).unwrap_or_else(|_| toplevel.clone());
            let prefix = canonical_root
                .strip_prefix(&canonical_top)
                .map(Path::to_path_buf)
                .map_err(|_| {
                    GitError::new(
                        alias,
                        format!(
                            "индексный корень '{}' находится вне рабочего дерева git '{}'",
                            index_root.display(),
                            toplevel.display()
                        ),
                    )
                    .with_configured_path(&configured)
                    .with_hint("git_path должен указывать на дерево, содержащее корень индекса")
                })?;
            (Some(toplevel), prefix)
        };

        let shallow_out = run_git(
            Some(configured.clone()),
            vec!["rev-parse".into(), "--is-shallow-repository".into()],
        )
        .await
        .map_err(|e| {
            fail(
                format!("не удалось проверить полноту истории: {e}"),
                e.clone(),
            )
        })?;
        if shallow_out.success && shallow_out.trimmed() == "true" {
            return Err(GitError::new(
                alias,
                "история репозитория обрезана (shallow clone): git-инструменты требуют полной истории",
            )
            .with_configured_path(&configured)
            .with_hint("выполните `git fetch --unshallow` в клоне; сам инструмент fetch не запускает"));
        }

        Ok(Self {
            alias: alias.to_string(),
            index_root,
            git_dir,
            work_tree,
            prefix,
            bare,
        })
    }

    /// Запуск разрешённой команды в координатах репозитория. Ненулевой код
    /// возврата превращается в структурированную ошибку.
    async fn git(&self, args: Vec<String>) -> Result<GitOutput, GitError> {
        validate_git_args(&args)
            .map_err(|e| GitError::new(&self.alias, format!("недопустимые аргументы git: {e}")))?;
        let mut full = Vec::with_capacity(args.len() + 1);
        if self.bare {
            full.push(format!("--git-dir={}", self.git_dir.display()));
        }
        full.extend(args);
        let cwd = if self.bare {
            None
        } else {
            Some(self.index_root.clone())
        };
        let out = run_git(cwd, full)
            .await
            .map_err(|e| GitError::new(&self.alias, e).with_stderr(""))?;
        if !out.success {
            return Err(GitError::new(&self.alias, "git завершился с ошибкой")
                .with_stderr(out.stderr_trimmed()));
        }
        Ok(out)
    }

    /// Как [`GitContext::git`], но ненулевой код возврата — это `None`
    /// (нужно для `rev-parse --verify --quiet`).
    async fn git_optional(&self, args: Vec<String>) -> Result<Option<GitOutput>, GitError> {
        validate_git_args(&args)
            .map_err(|e| GitError::new(&self.alias, format!("недопустимые аргументы git: {e}")))?;
        let mut full = Vec::with_capacity(args.len() + 1);
        if self.bare {
            full.push(format!("--git-dir={}", self.git_dir.display()));
        }
        full.extend(args);
        let cwd = if self.bare {
            None
        } else {
            Some(self.index_root.clone())
        };
        let out = run_git(cwd, full)
            .await
            .map_err(|e| GitError::new(&self.alias, e))?;
        if out.success {
            Ok(Some(out))
        } else {
            Ok(None)
        }
    }

    /// Флаги выдачи путей: для рабочего дерева — `--relative`, чтобы пути
    /// оставались в индексных координатах. Bare работает от корня репозитория.
    fn relative_flag(&self) -> Option<String> {
        if self.bare {
            None
        } else {
            Some("--relative".to_string())
        }
    }

    /// Индексные координаты пути из выдачи git. Для рабочего дерева выдача
    /// уже относительна индексного корня; для bare путь идёт от корня
    /// репозитория — это документированное исключение.
    fn index_coords(&self, git_path: &str) -> String {
        git_path.to_string()
    }

    /// Путь от корня репозитория — то, что понимает `<rev>:<path>` у `show`.
    fn repo_path(&self, index_path: &str) -> String {
        if self.bare || self.prefix.as_os_str().is_empty() {
            return index_path.to_string();
        }
        let joined = self.prefix.join(index_path);
        joined
            .components()
            .filter_map(|c| match c {
                Component::Normal(part) => Some(part.to_string_lossy().replace('\\', "/")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/")
    }
}

/// `rev-parse` одного пути (`--show-toplevel`, `--absolute-git-dir`).
async fn rev_parse_path(
    configured: &Path,
    flag: &str,
    alias: &str,
    fail: &impl Fn(String, String) -> GitError,
) -> Result<PathBuf, GitError> {
    let out = run_git(
        Some(configured.to_path_buf()),
        vec!["rev-parse".into(), flag.into()],
    )
    .await
    .map_err(|e| {
        fail(
            format!("не удалось выполнить rev-parse {flag}: {e}"),
            e.clone(),
        )
    })?;
    if !out.success {
        return Err(fail(
            format!("rev-parse {flag} завершился с ошибкой"),
            out.stderr_trimmed(),
        ));
    }
    let path = out.trimmed();
    if path.is_empty() {
        return Err(
            GitError::new(alias, format!("rev-parse {flag} вернул пустой путь"))
                .with_configured_path(configured),
        );
    }
    Ok(PathBuf::from(path))
}

// ── Валидация путей и ревизий ──────────────────────────────────────────────

/// Путь инструмента должен быть буквальным индексным путём: без абсолютных
/// префиксов, NUL и выхода через `..`. Исторический путь при этом не обязан
/// существовать на диске.
fn validate_index_path(repo: &str, path: &str) -> Result<(), GitError> {
    if path.is_empty() {
        return Err(GitError::new(repo, "путь не может быть пустым"));
    }
    if path.contains('\0') {
        return Err(GitError::new(repo, "путь содержит NUL").with_path(path));
    }
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err(GitError::new(
            repo,
            "абсолютный путь не принимается: нужен путь от корня индекса",
        )
        .with_path(path));
    }
    for component in candidate.components() {
        match component {
            Component::ParentDir => {
                return Err(
                    GitError::new(repo, "путь не может выходить за корень через '..'")
                        .with_path(path),
                );
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(GitError::new(repo, "абсолютный путь не принимается").with_path(path));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Ревизия не должна выглядеть опцией git: `--foo` отвергаем до запуска.
fn validate_rev_input(repo: &str, input: &str) -> Result<(), GitError> {
    if input.trim().is_empty() {
        return Err(GitError::new(repo, "пустая ревизия"));
    }
    if input.starts_with('-') {
        return Err(GitError::new(repo, "ревизия не может начинаться с '-'").with_input(input));
    }
    Ok(())
}

// ── Разрешение ревизий и дат ───────────────────────────────────────────────

/// Раскрытая ревизия (или дата, приведённая к коммиту).
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedRevision {
    /// Исходный ввод как его передал клиент.
    pub input: String,
    /// Полный хеш коммита.
    pub resolved_hash: String,
    /// Совместимый дубликат `resolved_hash` под ожидаемым именем `hash`.
    pub hash: String,
    /// Дата коммита в ISO-8601 с смещением.
    pub date: String,
    /// Тема коммита.
    pub subject: String,
    /// `"rev"` — ввод был ревизией; `"date"` — ввод раскрыт как локальная дата.
    pub resolved_as: String,
    /// Часовой пояс исполняющей машины.
    pub timezone: String,
    /// Фактический момент раскрытия даты (ISO-8601 с смещением).
    pub resolved_at: String,
}

/// Сведения о коммите для ответа.
async fn commit_info(ctx: &GitContext, hash: &str) -> Result<(String, String), GitError> {
    let out = ctx
        .git(vec![
            "show".into(),
            "-s".into(),
            "--no-textconv".into(),
            "--format=%cI%x1f%s".into(),
            hash.to_string(),
        ])
        .await?;
    let text = out.text();
    let line = text.lines().next().unwrap_or("");
    let mut parts = line.splitn(2, '\u{1f}');
    let date = parts.next().unwrap_or("").to_string();
    let subject = parts.next().unwrap_or("").to_string();
    Ok((date, subject))
}

fn resolved_by_rev(input: &str, hash: String, date: String, subject: String) -> ResolvedRevision {
    let now = Local::now();
    ResolvedRevision {
        input: input.to_string(),
        resolved_hash: hash.clone(),
        hash,
        date,
        subject,
        resolved_as: "rev".to_string(),
        timezone: now.offset().to_string(),
        resolved_at: now.format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
    }
}

/// Разобрать локальную дату строго в одном из трёх форматов.
fn parse_local_moment(
    repo: &str,
    input: &str,
    end_of_day: bool,
) -> Result<chrono::DateTime<Local>, GitError> {
    let naive: NaiveDateTime = if let Ok(date) = NaiveDate::parse_from_str(input, "%Y-%m-%d") {
        let time = if end_of_day {
            date.and_hms_opt(23, 59, 59)
        } else {
            date.and_hms_opt(0, 0, 0)
        };
        time.ok_or_else(|| GitError::new(repo, format!("невозможная дата '{input}'")))?
    } else if let Ok(dt) = NaiveDateTime::parse_from_str(input, "%Y-%m-%d %H:%M") {
        dt
    } else if let Ok(dt) = NaiveDateTime::parse_from_str(input, "%Y-%m-%dT%H:%M") {
        dt
    } else {
        return Err(GitError::new(
                repo,
                format!(
                    "не удалось разобрать '{input}': ожидается ревизия, YYYY-MM-DD, YYYY-MM-DD HH:MM или YYYY-MM-DDTHH:MM"
                ),
            )
            .with_input(input));
    };
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(dt) => Ok(dt),
        LocalResult::Ambiguous(_, _) => Err(GitError::new(
            repo,
            format!("локальный момент '{input}' неоднозначен из-за перехода часового пояса"),
        )
        .with_input(input)
        .with_hint("укажите дату без времени или другое время")),
        LocalResult::None => Err(GitError::new(
            repo,
            format!("локальный момент '{input}' не существует из-за перехода часового пояса"),
        )
        .with_input(input)),
    }
}

/// Раскрыть ревизию или дату в коммит.
///
/// Сначала ввод проверяется как ревизия (`rev-parse --verify …^{commit}`) —
/// тег с именем даты имеет приоритет над разбором даты. Иначе ввод строго
/// разбирается как локальный момент; коммит ищется согласованным
/// `rev-list -1 --before=<момент> <upper>`. Для `to` верхняя граница — HEAD,
/// для `from` — уже раскрытый `to`.
pub async fn resolve_revision(
    ctx: &GitContext,
    input: &str,
    upper: &str,
    end_of_day: bool,
) -> Result<ResolvedRevision, GitError> {
    validate_rev_input(&ctx.alias, input)?;
    let verify = ctx
        .git_optional(vec![
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("{input}^{{commit}}"),
        ])
        .await?;
    if let Some(out) = verify {
        let hash = out.trimmed();
        if !hash.is_empty() {
            let (date, subject) = commit_info(ctx, &hash).await?;
            return Ok(resolved_by_rev(input, hash, date, subject));
        }
    }

    let moment = parse_local_moment(&ctx.alias, input, end_of_day)?;
    let moment_text = moment.format("%Y-%m-%dT%H:%M:%S%:z").to_string();
    let out = ctx
        .git(vec![
            "rev-list".into(),
            "-1".into(),
            format!("--before={moment_text}"),
            upper.to_string(),
        ])
        .await?;
    let hash = out.trimmed();
    if hash.is_empty() {
        return Err(GitError::new(
            &ctx.alias,
            format!("для даты '{input}' не найден коммит не позже {moment_text}"),
        )
        .with_input(input)
        .with_upper_bound(upper)
        .with_hint(
            "дата раскрывается в локальной зоне исполняющей машины; проверьте дату и границу",
        ));
    }
    let (date, subject) = commit_info(ctx, &hash).await?;
    Ok(ResolvedRevision {
        input: input.to_string(),
        resolved_hash: hash.clone(),
        hash,
        date,
        subject,
        resolved_as: "date".to_string(),
        timezone: moment.offset().to_string(),
        resolved_at: Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
    })
}

/// Пара снимков `from`/`to`. `to` раскрывается первым, `from` — относительно
/// уже раскрытого `to`; неявный merge-base не используется.
pub async fn resolve_range(
    ctx: &GitContext,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<(ResolvedRevision, Option<ResolvedRevision>), GitError> {
    let to_input = to.unwrap_or("HEAD");
    let to_rev = resolve_revision(ctx, to_input, "HEAD", true).await?;
    let from_rev = match from {
        Some(input) => Some(resolve_revision(ctx, input, &to_rev.resolved_hash, false).await?),
        None => None,
    };
    Ok((to_rev, from_rev))
}

// ── Изменённые файлы ───────────────────────────────────────────────────────

/// Один изменённый файл в индексных координатах.
#[derive(Debug, Clone, Serialize)]
pub struct ChangedFile {
    /// Новый путь; для удаления — старый (единственный существующий).
    pub path: String,
    /// Старый путь переименования/копирования.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// Статус git: A, M, D, R, C. Изменение типа (T) приводится к M.
    pub status: String,
    pub insertions: u64,
    pub deletions: u64,
    pub binary: bool,
}

/// Собрать полный список изменений между двумя снимками.
///
/// Статусы берутся из `diff --name-status -z` (распознавание переименований),
/// числовой вклад — из `diff --numstat -z`. NUL-разделители сохраняют
/// кириллицу, пробелы, табуляции и переводы строк в именах.
pub async fn collect_changed_files(
    ctx: &GitContext,
    from: &str,
    to: &str,
) -> Result<Vec<ChangedFile>, GitError> {
    let mut base = vec!["diff".to_string(), "--find-renames".to_string()];
    base.push("--no-ext-diff".into());
    base.push("--no-textconv".into());
    if let Some(flag) = ctx.relative_flag() {
        base.push(flag);
    }

    let mut status_args = base.clone();
    status_args.push("--name-status".into());
    status_args.push("-z".into());
    status_args.push(from.to_string());
    status_args.push(to.to_string());
    status_args.push("--".into());
    let status_out = ctx.git(status_args).await?;

    let mut numstat_args = base;
    numstat_args.push("--numstat".into());
    numstat_args.push("-z".into());
    numstat_args.push(from.to_string());
    numstat_args.push(to.to_string());
    numstat_args.push("--".into());
    let numstat_out = ctx.git(numstat_args).await?;

    let stats = parse_numstat(&numstat_out.stdout);
    let mut files = parse_name_status(&status_out.stdout);
    for file in files.iter_mut() {
        if let Some(stat) = stats.get(&file.path) {
            file.insertions = stat.insertions;
            file.deletions = stat.deletions;
            file.binary = stat.binary;
        }
    }
    files.sort_by(|a, b| {
        (a.path.as_str(), a.old_path.as_deref()).cmp(&(b.path.as_str(), b.old_path.as_deref()))
    });
    Ok(files)
}

#[derive(Debug, Clone, Copy, Default)]
struct FileStat {
    insertions: u64,
    deletions: u64,
    binary: bool,
}

fn parse_name_status(bytes: &[u8]) -> Vec<ChangedFile> {
    let parts: Vec<String> = bytes
        .split(|b| *b == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| String::from_utf8_lossy(chunk).to_string())
        .collect();
    let mut out = Vec::new();
    let mut index = 0;
    while index < parts.len() {
        let status_token = &parts[index];
        let status = status_token.chars().next().unwrap_or('M');
        if status == 'R' || status == 'C' {
            if index + 2 >= parts.len() {
                break;
            }
            let old_path = parts[index + 1].clone();
            let new_path = parts[index + 2].clone();
            out.push(ChangedFile {
                path: new_path,
                old_path: Some(old_path),
                status: status.to_string(),
                insertions: 0,
                deletions: 0,
                binary: false,
            });
            index += 3;
            continue;
        }
        if index + 1 >= parts.len() {
            break;
        }
        let path = parts[index + 1].clone();
        // Изменение типа (симлинк ↔ файл) считаем обычной модификацией.
        let normalized = if status == 'T' { 'M' } else { status };
        out.push(ChangedFile {
            path,
            old_path: None,
            status: normalized.to_string(),
            insertions: 0,
            deletions: 0,
            binary: false,
        });
        index += 2;
    }
    out
}

fn parse_numstat(bytes: &[u8]) -> BTreeMap<String, FileStat> {
    let parts: Vec<String> = bytes
        .split(|b| *b == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| String::from_utf8_lossy(chunk).to_string())
        .collect();
    let mut stats = BTreeMap::new();
    let mut index = 0;
    while index < parts.len() {
        let chunk = &parts[index];
        let mut fields = chunk.splitn(3, '\t');
        let added = fields.next().unwrap_or("");
        let deleted = fields.next().unwrap_or("");
        let path = fields.next().unwrap_or("");
        if path.is_empty() {
            // Переименование: пустое имя, далее старый и новый пути.
            if index + 2 >= parts.len() {
                break;
            }
            let new_path = parts[index + 2].clone();
            stats.insert(new_path, make_stat(added, deleted));
            index += 3;
            continue;
        }
        stats.insert(path.to_string(), make_stat(added, deleted));
        index += 1;
    }
    stats
}

fn make_stat(added: &str, deleted: &str) -> FileStat {
    if added == "-" || deleted == "-" {
        return FileStat {
            insertions: 0,
            deletions: 0,
            binary: true,
        };
    }
    FileStat {
        insertions: added.parse().unwrap_or(0),
        deletions: deleted.parse().unwrap_or(0),
        binary: false,
    }
}

// ── Вспомогательное ────────────────────────────────────────────────────────

/// Обрезка по Unicode-символам: граница не рвёт UTF-8.
fn truncate_chars(text: &str, max: usize) -> (String, bool) {
    let mut out = String::new();
    for (count, ch) in text.chars().enumerate() {
        if count >= max {
            return (out, true);
        }
        out.push(ch);
    }
    (out, false)
}

fn positive_limit(
    repo: &str,
    value: Option<usize>,
    default: usize,
    field: &str,
) -> Result<usize, GitError> {
    let limit = value.unwrap_or(default);
    if limit == 0 {
        return Err(GitError::new(
            repo,
            format!("{field} должен быть положительным"),
        ));
    }
    Ok(limit)
}

fn empty_range(from: &ResolvedRevision, to: &ResolvedRevision) -> bool {
    from.resolved_hash == to.resolved_hash
}

fn ok_json(value: serde_json::Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|e| format!("{{\"error\":\"сериализация: {e}\"}}"))
}

// ── git_log ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct LogCommit {
    hash: String,
    short: String,
    date: String,
    author: String,
    subject: String,
}

/// История коммитов ревизии (по умолчанию HEAD) с необязательным фильтром по
/// буквальному индексному пути. `total` считается по всей подходящей истории
/// до обрезки `limit`.
pub async fn git_log(entry: &RepoEntry, params: super::GitLogParams) -> String {
    let ctx = match GitContext::discover(&params.repo, entry).await {
        Ok(ctx) => ctx,
        Err(e) => return e.to_json(),
    };
    let limit = match positive_limit(&ctx.alias, params.limit, 20, "limit") {
        Ok(limit) => limit,
        Err(e) => return e.to_json(),
    };
    if let Some(path) = params.path.as_deref() {
        if let Err(e) = validate_index_path(&ctx.alias, path) {
            return e.to_json();
        }
    }
    let rev_input = params.rev.as_deref().unwrap_or("HEAD");
    let rev = match resolve_revision(&ctx, rev_input, "HEAD", true).await {
        Ok(rev) => rev,
        Err(e) => return e.to_json(),
    };

    let mut args = vec![
        "log".to_string(),
        "--format=%H%x1f%h%x1f%cI%x1f%an%x1f%s".to_string(),
    ];
    if let Some(flag) = ctx.relative_flag() {
        args.push(flag);
    }
    args.push(rev.resolved_hash.clone());
    args.push("--".to_string());
    if let Some(path) = params.path.as_deref() {
        args.push(path.to_string());
    }
    let out = match ctx.git(args).await {
        Ok(out) => out,
        Err(e) => return e.to_json(),
    };

    let mut commits = Vec::new();
    let mut total = 0usize;
    for line in out.text().lines().filter(|l| !l.is_empty()) {
        total += 1;
        if commits.len() >= limit {
            continue;
        }
        let mut fields = line.splitn(5, '\u{1f}');
        let hash = fields.next().unwrap_or("").to_string();
        let short = fields.next().unwrap_or("").to_string();
        let date = fields.next().unwrap_or("").to_string();
        let author = fields.next().unwrap_or("").to_string();
        let subject = fields.next().unwrap_or("").to_string();
        commits.push(LogCommit {
            hash,
            short,
            date,
            author,
            subject,
        });
    }
    let truncated = total > commits.len();
    let shown = commits.len();
    let mut response = serde_json::json!({
        "repo": ctx.alias,
        "rev": rev,
        "commits": commits,
        "shown": shown,
        "total": total,
        "limit": limit,
        "truncated": truncated,
    });
    if truncated {
        response["hint"] = serde_json::Value::String(
            "история обрезана по limit: увеличьте limit или сузьте path/rev".to_string(),
        );
    }
    ok_json(response)
}

// ── git_changed_files ──────────────────────────────────────────────────────

/// Изменённые файлы между двумя снимками. `files_changed` и суммы вклада
/// считаются после фильтра `path_glob`, но до обрезки `limit`.
pub async fn git_changed_files(entry: &RepoEntry, params: super::GitChangedFilesParams) -> String {
    let ctx = match GitContext::discover(&params.repo, entry).await {
        Ok(ctx) => ctx,
        Err(e) => return e.to_json(),
    };
    let limit = match positive_limit(&ctx.alias, params.limit, 200, "limit") {
        Ok(limit) => limit,
        Err(e) => return e.to_json(),
    };
    let (to_rev, from_rev) =
        match resolve_range(&ctx, params.from.as_deref(), params.to.as_deref()).await {
            Ok(pair) => pair,
            Err(e) => return e.to_json(),
        };
    let from_rev = match from_rev {
        Some(rev) => rev,
        None => to_rev.clone(),
    };
    if empty_range(&from_rev, &to_rev) {
        return ok_json(serde_json::json!({
            "repo": ctx.alias,
            "from": from_rev,
            "to": to_rev,
            "empty_range": true,
            "files": [],
            "files_changed": 0,
            "insertions": 0,
            "deletions": 0,
            "shown": 0,
            "limit": limit,
            "truncated": false,
        }));
    }
    let files =
        match collect_changed_files(&ctx, &from_rev.resolved_hash, &to_rev.resolved_hash).await {
            Ok(files) => files,
            Err(e) => return e.to_json(),
        };
    let matcher = match params.path_glob.as_deref() {
        Some(pattern) => match globset::Glob::new(pattern) {
            Ok(glob) => Some(glob.compile_matcher()),
            Err(e) => {
                return GitError::new(&ctx.alias, format!("некорректный path_glob: {e}")).to_json()
            }
        },
        None => None,
    };
    let filtered: Vec<ChangedFile> = files
        .into_iter()
        .filter(|file| match &matcher {
            Some(matcher) => matcher.is_match(&file.path),
            None => true,
        })
        .collect();
    let files_changed = filtered.len();
    let insertions: u64 = filtered.iter().map(|f| f.insertions).sum();
    let deletions: u64 = filtered.iter().map(|f| f.deletions).sum();
    let truncated = files_changed > limit;
    let shown_files: Vec<ChangedFile> = filtered.into_iter().take(limit).collect();
    let shown = shown_files.len();
    let mut response = serde_json::json!({
        "repo": ctx.alias,
        "from": from_rev,
        "to": to_rev,
        "empty_range": false,
        "files": shown_files,
        "files_changed": files_changed,
        "insertions": insertions,
        "deletions": deletions,
        "shown": shown,
        "limit": limit,
        "truncated": truncated,
    });
    if truncated {
        response["hint"] = serde_json::Value::String(
            "список обрезан по limit: увеличьте limit или сузьте path_glob/диапазон".to_string(),
        );
    }
    ok_json(response)
}

// ── git_changes_by_object ──────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
struct ObjectAcc {
    object_name: Option<String>,
    meta_type: Option<String>,
    files: Vec<ChangedFile>,
}

/// Найти объекты метаданных для изменённых путей пакетными запросами.
///
/// Таблица принадлежит bsl-extension: `code_path` относителен корня индекса,
/// `object_name` — множественное имя категории, колонки `meta_type` нет.
/// Отсутствие таблицы (не-BSL репозиторий) не ошибка — вызывающая сторона
/// разберёт раскладку сама.
async fn metadata_objects(entry: &RepoEntry, paths: &BTreeSet<String>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if paths.is_empty() {
        return out;
    }
    // Локальная запись без пула (тесты, служебные сборки) — просто без
    // сопоставления: разбор раскладки выполняет вызывающая сторона.
    let Some(pool) = entry.storage.as_ref() else {
        return out;
    };
    let storage = match pool.get().await {
        Ok(storage) => storage,
        Err(_) => return out,
    };
    let conn = storage.conn();
    let values: Vec<String> = paths.iter().cloned().collect();
    for chunk in values.chunks(METADATA_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT code_path, object_name FROM metadata_modules \
             WHERE repo = ? AND code_path IN ({placeholders})"
        );
        let mut stmt = match conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(_) => return out,
        };
        let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() + 1);
        // Записи bsl-extension сохраняются под REPO_DEFAULT, а не под
        // MCP-алиасом репозитория.
        let repo_value: String = "default".to_string();
        params_vec.push(&repo_value);
        for path in chunk {
            params_vec.push(path);
        }
        let rows = stmt.query_map(rusqlite::params_from_iter(params_vec), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        });
        let rows = match rows {
            Ok(rows) => rows,
            Err(_) => return out,
        };
        for row in rows.flatten() {
            out.insert(row.0, row.1);
        }
    }
    out
}

/// Разобрать известную раскладку выгрузки 1С. Неизвестные и неоднозначные
/// пути остаются без объекта (`None`).
fn parse_layout(path: &str) -> Option<(String, Option<String>)> {
    let normalized = path.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 3 {
        return None;
    }
    // Расширения: extensions/<Имя>/… — группируем по имени расширения.
    // meta_type помечает, что это расширение, а не объект основной
    // конфигурации: имя расширения с именем объекта не путается.
    if parts[0] == "extensions" {
        return Some((parts[1].to_string(), Some("Extension".to_string())));
    }
    if parts[0] == "External" {
        return Some((parts[1].to_string(), None));
    }
    if parts[0] == "base" && parts.len() >= 3 {
        // Неизвестная категория — не повод выдумывать тип: такой путь
        // уходит в ungrouped, как и любой нераспознанный.
        let meta_type = meta_type_for_category(parts[1])?;
        return Some((parts[2].to_string(), Some(meta_type)));
    }
    None
}

/// Изменения, сгруппированные по объектам метаданных. Полный список изменений
/// обрабатывается до обрезки объектов; `totals` и `objects_total` всегда
/// полные, пути отдаются только при `include_files` и с отметкой обрезки.
pub async fn git_changes_by_object(
    entry: &RepoEntry,
    params: super::GitChangesByObjectParams,
) -> String {
    let ctx = match GitContext::discover(&params.repo, entry).await {
        Ok(ctx) => ctx,
        Err(e) => return e.to_json(),
    };
    let limit = match positive_limit(&ctx.alias, params.limit, 100, "limit") {
        Ok(limit) => limit,
        Err(e) => return e.to_json(),
    };
    let include_files = params.include_files.unwrap_or(false);
    let (to_rev, from_rev) =
        match resolve_range(&ctx, params.from.as_deref(), params.to.as_deref()).await {
            Ok(pair) => pair,
            Err(e) => return e.to_json(),
        };
    let from_rev = match from_rev {
        Some(rev) => rev,
        None => to_rev.clone(),
    };
    if empty_range(&from_rev, &to_rev) {
        return ok_json(serde_json::json!({
            "repo": ctx.alias,
            "from": from_rev,
            "to": to_rev,
            "empty_range": true,
            "objects": [],
            "objects_total": 0,
            "ungrouped": {"files_changed": 0, "insertions": 0, "deletions": 0},
            "totals": {"files_changed": 0, "insertions": 0, "deletions": 0, "objects_total": 0},
            "shown": 0,
            "limit": limit,
            "truncated": false,
        }));
    }
    let files =
        match collect_changed_files(&ctx, &from_rev.resolved_hash, &to_rev.resolved_hash).await {
            Ok(files) => files,
            Err(e) => return e.to_json(),
        };
    let totals_files = files.len();
    let totals_insertions: u64 = files.iter().map(|f| f.insertions).sum();
    let totals_deletions: u64 = files.iter().map(|f| f.deletions).sum();

    let code_paths: BTreeSet<String> = files
        .iter()
        .map(|file| ctx.index_coords(&file.path))
        .collect();
    let metadata = metadata_objects(entry, &code_paths).await;

    let mut groups: BTreeMap<(Option<String>, Option<String>), ObjectAcc> = BTreeMap::new();
    // Несопоставленные пути не становятся объектами: имени у них нет, и
    // придумывать его нельзя. Они идут отдельным блоком `ungrouped`.
    let mut ungrouped_files: Vec<ChangedFile> = Vec::new();
    for file in files {
        let index_path = ctx.index_coords(&file.path);
        let matched = match metadata.get(&index_path) {
            // object_name записан как есть (множественное имя категории);
            // meta_type — только из известного соответствия категории.
            Some(name) => Some((Some(name.clone()), meta_type_for_category(name))),
            None => parse_layout(&index_path).map(|(name, meta_type)| (Some(name), meta_type)),
        };
        let Some((object_name, meta_type)) = matched else {
            ungrouped_files.push(file);
            continue;
        };
        let key = (object_name.clone(), meta_type.clone());
        groups
            .entry(key)
            .or_insert_with(|| ObjectAcc {
                object_name,
                meta_type,
                files: Vec::new(),
            })
            .files
            .push(file);
    }
    let ungrouped_count = ungrouped_files.len();
    let ungrouped_insertions: u64 = ungrouped_files.iter().map(|f| f.insertions).sum();
    let ungrouped_deletions: u64 = ungrouped_files.iter().map(|f| f.deletions).sum();

    let objects_total = groups.len();
    let truncated = objects_total > limit;
    let mut objects = Vec::new();
    let mut paths_truncated = false;
    for acc in groups.into_values().take(limit) {
        let object_files = acc.files.len();
        let insertions: u64 = acc.files.iter().map(|f| f.insertions).sum();
        let deletions: u64 = acc.files.iter().map(|f| f.deletions).sum();
        let mut object = serde_json::json!({
            "object_name": acc.object_name,
            "meta_type": acc.meta_type,
            "files_changed": object_files,
            "insertions": insertions,
            "deletions": deletions,
        });
        if include_files {
            let mut paths: Vec<String> = acc.files.iter().map(|f| f.path.clone()).collect();
            let paths_total = paths.len();
            let paths_cut = paths_total > PATHS_PER_OBJECT;
            paths.truncate(PATHS_PER_OBJECT);
            if paths_cut {
                paths_truncated = true;
            }
            object["paths"] = serde_json::Value::Array(
                paths.into_iter().map(serde_json::Value::String).collect(),
            );
            object["paths_total"] = serde_json::json!(paths_total);
            object["paths_truncated"] = serde_json::json!(paths_cut);
        }
        objects.push(object);
    }
    let any_truncated = truncated || paths_truncated;
    let shown = objects.len();
    let mut response = serde_json::json!({
        "repo": ctx.alias,
        "from": from_rev,
        "to": to_rev,
        "empty_range": false,
        "objects": objects,
        "objects_total": objects_total,
        "ungrouped": {
            "files_changed": ungrouped_count,
            "insertions": ungrouped_insertions,
            "deletions": ungrouped_deletions,
        },
        "totals": {
            "files_changed": totals_files,
            "insertions": totals_insertions,
            "deletions": totals_deletions,
            "objects_total": objects_total,
        },
        "shown": shown,
        "limit": limit,
        "truncated": any_truncated,
    });
    if any_truncated {
        response["hint"] = serde_json::Value::String(
            "выдача обрезана: увеличьте limit, сузьте диапазон или запросите include_files=false"
                .to_string(),
        );
    }
    ok_json(response)
}

/// Есть ли путь в указанной ревизии: `<rev>:<path>` разрешается в объект.
async fn path_exists_in(ctx: &GitContext, rev: &str, repo_path: &str) -> Result<bool, GitError> {
    let out = ctx
        .git_optional(vec![
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("{rev}:{repo_path}"),
        ])
        .await?;
    Ok(out.is_some())
}

// ── git_diff ───────────────────────────────────────────────────────────────

/// Diff одного буквального пути между двумя снимками.
///
/// Отсутствие пути в обоих снимках — ошибка; существующий неизменённый путь
/// даёт пустой патч. Для переименования в патч включаются обе стороны.
pub async fn git_diff(entry: &RepoEntry, params: super::GitDiffParams) -> String {
    let ctx = match GitContext::discover(&params.repo, entry).await {
        Ok(ctx) => ctx,
        Err(e) => return e.to_json(),
    };
    if let Err(e) = validate_index_path(&ctx.alias, &params.path) {
        return e.to_json();
    }
    let context_lines = params.context_lines.unwrap_or(3);
    let max_chars = match positive_limit(&ctx.alias, params.max_chars, 20_000, "max_chars") {
        Ok(value) => value,
        Err(e) => return e.to_json(),
    };
    let (to_rev, from_rev) =
        match resolve_range(&ctx, params.from.as_deref(), params.to.as_deref()).await {
            Ok(pair) => pair,
            Err(e) => return e.to_json(),
        };
    let from_rev = match from_rev {
        Some(rev) => rev,
        None => to_rev.clone(),
    };

    let repo_path = ctx.repo_path(&params.path);
    let in_to = match path_exists_in(&ctx, &to_rev.resolved_hash, &repo_path).await {
        Ok(value) => value,
        Err(e) => return e.to_json(),
    };
    let in_from = match path_exists_in(&ctx, &from_rev.resolved_hash, &repo_path).await {
        Ok(value) => value,
        Err(e) => return e.to_json(),
    };
    if !in_to && !in_from {
        return GitError::new(
            &ctx.alias,
            format!(
                "путь '{}' отсутствует в обоих снимках ({} → {})",
                params.path, from_rev.input, to_rev.input
            ),
        )
        .with_path(&params.path)
        .to_json();
    }

    let changes =
        match collect_changed_files(&ctx, &from_rev.resolved_hash, &to_rev.resolved_hash).await {
            Ok(changes) => changes,
            Err(e) => return e.to_json(),
        };
    let matched = changes.iter().find(|file| {
        file.path == params.path || file.old_path.as_deref() == Some(params.path.as_str())
    });

    if let Some(file) = matched.filter(|file| file.binary) {
        return ok_json(serde_json::json!({
            "repo": ctx.alias,
            "from": from_rev,
            "to": to_rev,
            "path": params.path,
            "status": file.status.clone(),
            "insertions": file.insertions,
            "deletions": file.deletions,
            "binary": true,
            "patch": "",
            "truncated": false,
        }));
    } else if empty_range(&from_rev, &to_rev) {
        return ok_json(serde_json::json!({
            "repo": ctx.alias,
            "from": from_rev,
            "to": to_rev,
            "path": params.path,
            "status": "unchanged",
            "insertions": 0,
            "deletions": 0,
            "binary": false,
            "patch": "",
            "truncated": false,
        }));
    }

    let mut args = vec![
        "diff".to_string(),
        format!("-U{context_lines}"),
        "--no-ext-diff".to_string(),
        "--no-textconv".to_string(),
    ];
    if let Some(flag) = ctx.relative_flag() {
        args.push(flag);
    }
    args.push(from_rev.resolved_hash.clone());
    args.push(to_rev.resolved_hash.clone());
    args.push("--".to_string());
    args.push(params.path.clone());
    if let Some(old) = matched.and_then(|file| file.old_path.as_ref()) {
        args.push(old.clone());
    }
    let out = match ctx.git(args).await {
        Ok(out) => out,
        Err(e) => return e.to_json(),
    };
    let binary = out.stdout.contains(&0);
    let raw = if binary {
        String::new()
    } else {
        // Недекодируемое содержимое не выдаём как текст.
        String::from_utf8(out.stdout).unwrap_or_default()
    };
    let (patch, truncated) = truncate_chars(&raw, max_chars);
    let status = matched
        .map(|f| f.status.clone())
        .unwrap_or_else(|| "unchanged".into());
    let insertions = matched.map(|f| f.insertions).unwrap_or(0);
    let deletions = matched.map(|f| f.deletions).unwrap_or(0);
    let mut response = serde_json::json!({
        "repo": ctx.alias,
        "from": from_rev,
        "to": to_rev,
        "path": params.path,
        "status": status,
        "insertions": insertions,
        "deletions": deletions,
        "binary": binary,
        "patch": patch,
        "max_chars": max_chars,
        "truncated": truncated,
    });
    if truncated {
        response["hint"] = serde_json::Value::String(
            "патч обрезан по max_chars: уменьшите context_lines или увеличьте max_chars"
                .to_string(),
        );
    }
    ok_json(response)
}

// ── git_show_file ──────────────────────────────────────────────────────────

/// Историческое содержимое файла (`show <rev>:<path>`).
///
/// Диапазон строк — 1-based inclusive. `lines_total` считается по файлу до
/// диапазона и обрезки: начало за концом файла даёт пустое содержимое, но не
/// теряет счётчик. Каталог, отсутствующий путь и бинарное содержимое —
/// структурированные ошибки.
pub async fn git_show_file(entry: &RepoEntry, params: super::GitShowFileParams) -> String {
    let ctx = match GitContext::discover(&params.repo, entry).await {
        Ok(ctx) => ctx,
        Err(e) => return e.to_json(),
    };
    if let Err(e) = validate_index_path(&ctx.alias, &params.path) {
        return e.to_json();
    }
    let max_chars = match positive_limit(&ctx.alias, params.max_chars, 20_000, "max_chars") {
        Ok(value) => value,
        Err(e) => return e.to_json(),
    };
    let line_start = params.line_start.unwrap_or(1);
    let line_end = params.line_end;
    if line_start == 0 {
        return GitError::new(&ctx.alias, "line_start должен быть >= 1")
            .with_path(&params.path)
            .to_json();
    }
    if let Some(end) = line_end {
        if end == 0 {
            return GitError::new(&ctx.alias, "line_end должен быть >= 1")
                .with_path(&params.path)
                .to_json();
        }
        if end < line_start {
            return GitError::new(&ctx.alias, "line_end не может быть меньше line_start")
                .with_path(&params.path)
                .to_json();
        }
    }

    let rev_input = params.rev.as_deref().unwrap_or("HEAD");
    let rev = match resolve_revision(&ctx, rev_input, "HEAD", true).await {
        Ok(rev) => rev,
        Err(e) => return e.to_json(),
    };
    let repo_path = ctx.repo_path(&params.path);
    let spec = format!("{}:{}", rev.resolved_hash, repo_path);
    let exists = match ctx
        .git_optional(vec![
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            spec.clone(),
        ])
        .await
    {
        Ok(value) => value.is_some(),
        Err(e) => return e.to_json(),
    };
    if !exists {
        return GitError::new(
            &ctx.alias,
            format!(
                "путь '{}' отсутствует в ревизии '{}'",
                params.path, rev.input
            ),
        )
        .with_path(&params.path)
        .to_json();
    }
    let is_dir = match ctx
        .git_optional(vec![
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("{spec}^{{tree}}"),
        ])
        .await
    {
        Ok(value) => value.is_some(),
        Err(e) => return e.to_json(),
    };
    if is_dir {
        return GitError::new(
            &ctx.alias,
            format!("'{}' — каталог, а не файл", params.path),
        )
        .with_path(&params.path)
        .to_json();
    }

    let out = match ctx
        .git(vec!["show".into(), "--no-textconv".into(), spec])
        .await
    {
        Ok(out) => out,
        Err(e) => return e.to_json(),
    };
    let bytes = out.stdout;
    if bytes.iter().take(8000).any(|b| *b == 0) {
        return GitError::new(
            &ctx.alias,
            format!(
                "'{}' — бинарное содержимое, не декодируется как текст",
                params.path
            ),
        )
        .with_path(&params.path)
        .to_json();
    }
    let content = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            return GitError::new(
                &ctx.alias,
                format!("'{}' не декодируется как UTF-8", params.path),
            )
            .with_path(&params.path)
            .to_json()
        }
    };
    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let lines_total = lines.len();
    let selected: Vec<&str> = if line_start > lines_total {
        Vec::new()
    } else {
        let end = line_end.unwrap_or(lines_total).min(lines_total);
        lines[(line_start - 1)..end].to_vec()
    };
    let joined = selected.join("\n");
    let lines_returned = selected.len();
    let (text, truncated) = truncate_chars(&joined, max_chars);
    let mut response = serde_json::json!({
        "repo": ctx.alias,
        "rev": rev,
        "path": params.path,
        "line_start": line_start,
        "line_end": line_end.unwrap_or(lines_total),
        "lines_total": lines_total,
        "lines_returned": lines_returned,
        "content": text,
        "max_chars": max_chars,
        "truncated": truncated,
    });
    if truncated {
        response["hint"] = serde_json::Value::String(
            "содержимое обрезано по max_chars: сузьте диапазон строк или увеличьте max_chars"
                .to_string(),
        );
    }
    ok_json(response)
}

// ── Тесты ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{
        GitChangedFilesParams, GitChangesByObjectParams, GitDiffParams, GitLogParams,
        GitShowFileParams, RepoEntry, RepoGitConfig,
    };
    use serde_json::Value;
    use std::path::Path;
    use std::process::Command;

    /// git есть в PATH? Без него проверки пропускаются, а не падают.
    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git запускается");
        assert!(
            out.status.success(),
            "git {:?} упал: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn write_file(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("каталог создаётся");
        }
        std::fs::write(path, text).expect("файл пишется");
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("временный каталог");
        run_git(dir.path(), &["init", "-b", "main"]);
        run_git(dir.path(), &["config", "user.name", "Тест"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "core.autocrlf", "false"]);
        dir
    }

    fn commit_at(dir: &Path, message: &str, date: &str) {
        run_git(dir, &["add", "-A"]);
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["commit", "-m", message])
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_AUTHOR_NAME", "Тест")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Тест")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("git commit");
        assert!(
            out.status.success(),
            "git commit упал: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn entry(root: &Path, enabled: bool) -> RepoEntry {
        RepoEntry {
            root_path: Some(root.to_path_buf()),
            storage: None,
            ip: "127.0.0.1".to_string(),
            port: 8011,
            is_local: true,
            language: Some("bsl".to_string()),
            processor: None,
            git_config: RepoGitConfig {
                enabled,
                git_path: None,
            },
        }
    }

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).expect("ответ инструмента — JSON")
    }

    fn log_params() -> GitLogParams {
        GitLogParams {
            repo: "demo".to_string(),
            rev: None,
            path: None,
            limit: None,
        }
    }

    fn changed(from: &str, to: &str) -> GitChangedFilesParams {
        GitChangedFilesParams {
            repo: "demo".to_string(),
            from: Some(from.to_string()),
            to: Some(to.to_string()),
            path_glob: None,
            limit: None,
        }
    }

    /// Два коммита с фиксированными датами: 2020-01-01 и 2020-01-02.
    fn two_commits(dir: &Path) {
        write_file(&dir.join("a.bsl"), "Процедура А()\nКонецПроцедуры\n");
        commit_at(dir, "первый", "2020-01-01T10:00:00+03:00");
        write_file(
            &dir.join("a.bsl"),
            "Процедура А()\n// правка\nКонецПроцедуры\n",
        );
        commit_at(dir, "второй", "2020-01-02T10:00:00+03:00");
    }

    #[tokio::test]
    async fn git_disabled() {
        let dir = init_repo();
        let out = parse(&git_log(&entry(dir.path(), false), log_params()).await);
        assert!(
            out["error"]
                .as_str()
                .unwrap_or_default()
                .contains("не включён"),
            "{out}"
        );
        assert!(
            out["hint"]
                .as_str()
                .unwrap_or_default()
                .contains("git = true"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn git_invalid_repository() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().expect("временный каталог");
        let out = parse(&git_log(&entry(dir.path(), true), log_params()).await);
        assert!(
            out["error"]
                .as_str()
                .unwrap_or_default()
                .contains("не удалось обнаружить git-репозиторий"),
            "{out}"
        );
        assert_eq!(
            out["configured_path"].as_str().unwrap_or_default(),
            dir.path().display().to_string()
        );
    }

    #[tokio::test]
    async fn git_shallow_repository() {
        if !git_available() {
            return;
        }
        let origin = init_repo();
        two_commits(origin.path());

        let holder = tempfile::tempdir().expect("временный каталог");
        let clone = holder.path().join("clone");
        let status = Command::new("git")
            .args(["clone", "--depth", "1", "--no-local"])
            .arg(origin.path())
            .arg(&clone)
            .output()
            .expect("git clone");
        assert!(
            status.status.success(),
            "git clone упал: {}",
            String::from_utf8_lossy(&status.stderr)
        );

        let out = parse(&git_log(&entry(&clone, true), log_params()).await);
        assert!(
            out["error"]
                .as_str()
                .unwrap_or_default()
                .contains("обрезана"),
            "{out}"
        );
        assert!(
            out["hint"]
                .as_str()
                .unwrap_or_default()
                .contains("--unshallow"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn git_paths_root() {
        if !git_available() {
            return;
        }
        let dir = init_repo();
        two_commits(dir.path());
        let out =
            parse(&git_changed_files(&entry(dir.path(), true), changed("HEAD~1", "HEAD")).await);
        assert_eq!(out["files_changed"].as_u64(), Some(1), "{out}");
        assert_eq!(out["files"][0]["path"].as_str(), Some("a.bsl"), "{out}");
        assert_eq!(out["files"][0]["status"].as_str(), Some("M"), "{out}");
        assert_eq!(out["files"][0]["insertions"].as_u64(), Some(1), "{out}");
    }

    #[tokio::test]
    async fn git_paths_subdirectory_roundtrip() {
        if !git_available() {
            return;
        }
        let dir = init_repo();
        write_file(&dir.path().join("base/x.bsl"), "Процедура Х()\n");
        commit_at(dir.path(), "первый", "2020-01-01T10:00:00+03:00");
        write_file(&dir.path().join("base/x.bsl"), "Процедура Х()\n// правка\n");
        commit_at(dir.path(), "второй", "2020-01-02T10:00:00+03:00");

        let index_root = dir.path().join("base");
        let out =
            parse(&git_changed_files(&entry(&index_root, true), changed("HEAD~1", "HEAD")).await);
        assert_eq!(out["files"][0]["path"].as_str(), Some("x.bsl"), "{out}");

        let shown = parse(
            &git_show_file(
                &entry(&index_root, true),
                GitShowFileParams {
                    repo: "demo".to_string(),
                    path: "x.bsl".to_string(),
                    rev: Some("HEAD".to_string()),
                    line_start: None,
                    line_end: None,
                    max_chars: None,
                },
            )
            .await,
        );
        assert!(
            shown["content"]
                .as_str()
                .unwrap_or_default()
                .contains("правка"),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn git_revision_before_date() {
        if !git_available() {
            return;
        }
        let dir = init_repo();
        two_commits(dir.path());
        let out = parse(
            &git_log(
                &entry(dir.path(), true),
                GitLogParams {
                    repo: "demo".to_string(),
                    rev: Some("2020-01-02".to_string()),
                    path: None,
                    limit: None,
                },
            )
            .await,
        );
        assert_eq!(out["rev"]["resolved_as"].as_str(), Some("date"), "{out}");
        assert_eq!(
            out["commits"][0]["subject"].as_str(),
            Some("второй"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn git_date_without_commit() {
        if !git_available() {
            return;
        }
        let dir = init_repo();
        two_commits(dir.path());
        let out = parse(
            &git_log(
                &entry(dir.path(), true),
                GitLogParams {
                    repo: "demo".to_string(),
                    rev: Some("2010-01-01".to_string()),
                    path: None,
                    limit: None,
                },
            )
            .await,
        );
        assert!(
            out["error"]
                .as_str()
                .unwrap_or_default()
                .contains("не найден коммит"),
            "{out}"
        );
        assert_eq!(out["input"].as_str(), Some("2010-01-01"), "{out}");
    }

    #[tokio::test]
    async fn git_empty_range() {
        if !git_available() {
            return;
        }
        let dir = init_repo();
        two_commits(dir.path());
        let out =
            parse(&git_changed_files(&entry(dir.path(), true), changed("HEAD", "HEAD")).await);
        assert_eq!(out["empty_range"].as_bool(), Some(true), "{out}");
        assert_eq!(out["files"].as_array().map(|a| a.len()), Some(0), "{out}");
    }

    #[tokio::test]
    async fn git_object_layout_fallback() {
        if !git_available() {
            return;
        }
        let dir = init_repo();
        let paths = [
            "base/Documents/Документ/Ext/ObjectModule.bsl",
            "External/Обработка/ExternalDataProcessor.obj.bsl",
            "extensions/Расширение/CommonModules/Модуль/Ext/Module.bsl",
            "base/НеизвестнаяКатегория/Объект/file.xml",
        ];
        for path in paths {
            write_file(&dir.path().join(path), "первая версия\n");
        }
        commit_at(dir.path(), "первый", "2020-01-01T10:00:00+03:00");
        for path in paths {
            write_file(&dir.path().join(path), "вторая версия\n");
        }
        commit_at(dir.path(), "второй", "2020-01-02T10:00:00+03:00");

        let out = parse(
            &git_changes_by_object(
                &entry(dir.path(), true),
                GitChangesByObjectParams {
                    repo: "demo".to_string(),
                    from: Some("HEAD~1".to_string()),
                    to: Some("HEAD".to_string()),
                    limit: None,
                    include_files: None,
                },
            )
            .await,
        );
        let objects = out["objects"].as_array().expect("объекты");
        assert!(
            objects
                .iter()
                .any(|o| o["object_name"] == "Документ" && o["meta_type"] == "Document"),
            "{out}"
        );
        assert!(
            objects
                .iter()
                .any(|o| o["object_name"] == "Обработка" && o["meta_type"].is_null()),
            "{out}"
        );
        assert!(
            objects
                .iter()
                .any(|o| o["object_name"] == "Расширение" && o["meta_type"] == "Extension"),
            "{out}"
        );
        assert_eq!(out["ungrouped"]["files_changed"].as_u64(), Some(1), "{out}");
        assert_eq!(out["totals"]["files_changed"].as_u64(), Some(4), "{out}");
    }

    #[tokio::test]
    async fn git_rename_binary_unicode() {
        if !git_available() {
            return;
        }
        let dir = init_repo();
        write_file(&dir.path().join("старый.bsl"), "Процедура А()\n");
        std::fs::write(dir.path().join("bin.dat"), [0u8, 1, 2, 3]).expect("бинарный файл");
        commit_at(dir.path(), "первый", "2020-01-01T10:00:00+03:00");

        run_git(dir.path(), &["mv", "старый.bsl", "новый.bsl"]);
        std::fs::write(dir.path().join("bin.dat"), [0u8, 7, 8, 9]).expect("бинарный файл");
        commit_at(dir.path(), "второй", "2020-01-02T10:00:00+03:00");

        let out =
            parse(&git_changed_files(&entry(dir.path(), true), changed("HEAD~1", "HEAD")).await);
        let files = out["files"].as_array().expect("файлы");
        let renamed = files
            .iter()
            .find(|f| f["path"] == "новый.bsl")
            .expect("переименование в выдаче");
        assert_eq!(renamed["status"].as_str(), Some("R"), "{out}");
        assert_eq!(renamed["old_path"].as_str(), Some("старый.bsl"), "{out}");
        let binary = files
            .iter()
            .find(|f| f["path"] == "bin.dat")
            .expect("бинарный файл в выдаче");
        assert_eq!(binary["binary"].as_bool(), Some(true), "{out}");
        assert_eq!(binary["insertions"].as_u64(), Some(0), "{out}");
    }

    #[tokio::test]
    async fn git_limits_preserve_totals() {
        if !git_available() {
            return;
        }
        let dir = init_repo();
        for name in ["a.bsl", "b.bsl", "c.bsl"] {
            write_file(&dir.path().join(name), "первая\n");
        }
        commit_at(dir.path(), "первый", "2020-01-01T10:00:00+03:00");
        for name in ["a.bsl", "b.bsl", "c.bsl"] {
            write_file(&dir.path().join(name), "вторая\n");
        }
        commit_at(dir.path(), "второй", "2020-01-02T10:00:00+03:00");

        let out = parse(
            &git_changed_files(
                &entry(dir.path(), true),
                GitChangedFilesParams {
                    repo: "demo".to_string(),
                    from: Some("HEAD~1".to_string()),
                    to: Some("HEAD".to_string()),
                    path_glob: None,
                    limit: Some(1),
                },
            )
            .await,
        );
        assert_eq!(out["files"].as_array().map(|a| a.len()), Some(1), "{out}");
        assert_eq!(out["files_changed"].as_u64(), Some(3), "{out}");
        assert_eq!(out["truncated"].as_bool(), Some(true), "{out}");
    }

    #[tokio::test]
    async fn git_show_line_ranges() {
        if !git_available() {
            return;
        }
        let dir = init_repo();
        write_file(&dir.path().join("a.txt"), "a\nb\nc\n");
        commit_at(dir.path(), "первый", "2020-01-01T10:00:00+03:00");

        let show = |start: Option<usize>, end: Option<usize>| GitShowFileParams {
            repo: "demo".to_string(),
            path: "a.txt".to_string(),
            rev: None,
            line_start: start,
            line_end: end,
            max_chars: None,
        };

        let out = parse(&git_show_file(&entry(dir.path(), true), show(Some(2), Some(3))).await);
        assert_eq!(out["content"].as_str(), Some("b\nc"), "{out}");
        assert_eq!(out["lines_total"].as_u64(), Some(3), "{out}");

        let beyond = parse(&git_show_file(&entry(dir.path(), true), show(Some(10), None)).await);
        assert_eq!(beyond["content"].as_str(), Some(""), "{beyond}");
        assert_eq!(beyond["lines_total"].as_u64(), Some(3), "{beyond}");

        let zero = parse(&git_show_file(&entry(dir.path(), true), show(Some(0), None)).await);
        assert!(
            zero["error"].as_str().unwrap_or_default().contains(">= 1"),
            "{zero}"
        );
    }

    #[tokio::test]
    async fn git_diff_missing_and_binary() {
        if !git_available() {
            return;
        }
        let dir = init_repo();
        write_file(&dir.path().join("a.bsl"), "Процедура А()\n");
        std::fs::write(dir.path().join("bin.dat"), [0u8, 1, 2]).expect("бинарный файл");
        commit_at(dir.path(), "первый", "2020-01-01T10:00:00+03:00");
        write_file(&dir.path().join("a.bsl"), "Процедура А()\n// правка\n");
        std::fs::write(dir.path().join("bin.dat"), [0u8, 4, 5]).expect("бинарный файл");
        commit_at(dir.path(), "второй", "2020-01-02T10:00:00+03:00");

        let diff = |path: &str| GitDiffParams {
            repo: "demo".to_string(),
            path: path.to_string(),
            from: Some("HEAD~1".to_string()),
            to: Some("HEAD".to_string()),
            context_lines: None,
            max_chars: None,
        };

        let out = parse(&git_diff(&entry(dir.path(), true), diff("a.bsl")).await);
        assert!(
            out["patch"]
                .as_str()
                .unwrap_or_default()
                .contains("+// правка"),
            "{out}"
        );

        let binary = parse(&git_diff(&entry(dir.path(), true), diff("bin.dat")).await);
        assert_eq!(binary["binary"].as_bool(), Some(true), "{binary}");
        assert_eq!(binary["patch"].as_str(), Some(""), "{binary}");

        let missing = parse(&git_diff(&entry(dir.path(), true), diff("нет-такого.bsl")).await);
        assert!(
            missing["error"]
                .as_str()
                .unwrap_or_default()
                .contains("отсутствует в обоих снимках"),
            "{missing}"
        );
    }
}
