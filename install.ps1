#Requires -Version 5.1
<#
.SYNOPSIS
    Установка code-index / bsl-indexer на Windows одной командой.

.DESCRIPTION
    Скачивает готовый архив нужного выпуска, распаковывает его в папку установки,
    запоминает эту папку в переменной окружения CODE_INDEX_HOME, создаёт заготовку
    файла настроек и печатает готовый блок для .mcp.json.

    По желанию регистрирует автозапуск: задачу для фонового индексатора и задачу
    для MCP-сервера, обе стартуют при входе в систему.

.PARAMETER InstallDir
    Куда положить программу. По умолчанию C:\tools\code-index.

.PARAMETER Flavor
    Какая сборка нужна: bsl — с поддержкой 1С, core — без неё.

.PARAMETER Version
    Версия выпуска: latest (по умолчанию) либо номер вида 1.0.0.

.PARAMETER Repo
    Папка с исходниками, которую сразу добавить в файл настроек. Формат
    `алиас=путь` либо просто путь (тогда алиас — default).

.PARAMETER Port
    Порт MCP-сервера, к которому подключается клиент. По умолчанию 8011.

.PARAMETER DaemonPort
    Порт фонового индексатора. По умолчанию 8015.

.PARAMETER RegisterAutostart
    Зарегистрировать автозапуск индексатора и сервера при входе в систему.
    По умолчанию — через папку автозагрузки текущего пользователя: прав
    администратора не требует, окна не показывает.

.PARAMETER UseScheduledTasks
    Регистрировать автозапуск задачами планировщика вместо папки автозагрузки.
    Требует запуска от имени администратора — планировщик иначе отказывает.

.PARAMETER TaskPrefix
    Начало имён задач автозапуска. По умолчанию CodeIndex, то есть задачи
    называются CodeIndexDaemon и CodeIndexServe. Меняйте, если на машине уже
    есть установка с такими именами.

.PARAMETER ReplaceTasks
    Перезаписать задачи автозапуска, если они уже существуют. Без этого ключа
    существующая задача остаётся нетронутой.

.PARAMETER NoPathUpdate
    Не добавлять папку установки в PATH пользователя.

.EXAMPLE
    irm https://raw.githubusercontent.com/Regsorm/code-index-mcp/main/install.ps1 | iex

.EXAMPLE
    & ([scriptblock]::Create((irm https://raw.githubusercontent.com/Regsorm/code-index-mcp/main/install.ps1))) -Repo "ut=C:\Repo1C" -RegisterAutostart
#>
[CmdletBinding()]
param(
    [string] $InstallDir = 'C:\tools\code-index',
    [ValidateSet('bsl', 'core')]
    [string] $Flavor = 'bsl',
    [string] $Version = 'latest',
    [string] $Repo,
    [int] $Port = 8011,
    [int] $DaemonPort = 8015,
    [switch] $RegisterAutostart,
    [switch] $UseScheduledTasks,
    [string] $TaskPrefix = 'CodeIndex',
    [switch] $ReplaceTasks,
    [switch] $NoPathUpdate
)

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$slug = 'Regsorm/code-index-mcp'
$asset = if ($Flavor -eq 'bsl') { 'bsl-indexer-windows-x64.zip' } else { 'code-index-windows-x64.zip' }
$exeName = if ($Flavor -eq 'bsl') { 'bsl-indexer.exe' } else { 'code-index.exe' }
$serverPort = $Port

function Write-Step([string] $Text) { Write-Host "==> $Text" -ForegroundColor Cyan }
function Write-Note([string] $Text) { Write-Host "    $Text" -ForegroundColor DarkGray }

# --- 1. Найти архив нужного выпуска -----------------------------------------

Write-Step "Ищу выпуск ($Version, сборка $Flavor)"

$apiUrl = if ($Version -eq 'latest') {
    "https://api.github.com/repos/$slug/releases/latest"
} else {
    "https://api.github.com/repos/$slug/releases/tags/v$($Version.TrimStart('v'))"
}

try {
    $release = Invoke-RestMethod -Uri $apiUrl -Headers @{ 'User-Agent' = 'code-index-install' }
} catch {
    throw "Не удалось получить сведения о выпуске по адресу $apiUrl. Проверьте доступ в сеть. Исходная ошибка: $($_.Exception.Message)"
}

$downloadUrl = $release.assets |
    Where-Object { $_.name -eq $asset } |
    Select-Object -First 1 -ExpandProperty browser_download_url

if (-not $downloadUrl) {
    throw "В выпуске $($release.tag_name) нет файла $asset. Доступны: $(($release.assets.name) -join ', ')"
}

Write-Note "выпуск $($release.tag_name), файл $asset"

# --- 2. Скачать и распаковать ------------------------------------------------

Write-Step "Скачиваю и распаковываю в $InstallDir"

$tmp = Join-Path ([IO.Path]::GetTempPath()) "code-index-$([guid]::NewGuid().ToString('N')).zip"
try {
    Invoke-WebRequest -Uri $downloadUrl -OutFile $tmp -UseBasicParsing
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Expand-Archive -Path $tmp -DestinationPath $InstallDir -Force
} finally {
    if (Test-Path $tmp) { Remove-Item $tmp -Force -ErrorAction SilentlyContinue }
}

$exePath = Join-Path $InstallDir $exeName
if (-not (Test-Path $exePath)) {
    $found = Get-ChildItem $InstallDir -Filter $exeName -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $found) { throw "После распаковки не найден $exeName в $InstallDir" }
    $exePath = $found.FullName
}

$installedVersion = (& $exePath --version) 2>&1
Write-Note "установлено: $installedVersion"

# --- 3. Переменная окружения и PATH -----------------------------------------

Write-Step 'Запоминаю папку установки'

[Environment]::SetEnvironmentVariable('CODE_INDEX_HOME', $InstallDir, 'User')
$env:CODE_INDEX_HOME = $InstallDir
Write-Note "CODE_INDEX_HOME = $InstallDir"

if (-not $NoPathUpdate) {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($userPath -notlike "*$InstallDir*") {
        $newPath = if ([string]::IsNullOrEmpty($userPath)) { $InstallDir } else { "$userPath;$InstallDir" }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Write-Note "папка добавлена в PATH пользователя (подхватится в новых окнах)"
    }
}

# --- 4. Заготовка файла настроек --------------------------------------------

$configPath = Join-Path $InstallDir 'daemon.toml'

if (Test-Path $configPath) {
    Write-Step 'Файл настроек уже есть — не трогаю'
    Write-Note $configPath
} else {
    Write-Step 'Создаю заготовку файла настроек'

    $pathsBlock = if ($Repo) {
        $alias, $dir = if ($Repo -match '^(?<a>[^=]+)=(?<d>.+)$') { $Matches.a, $Matches.d } else { 'default', $Repo }
        @"

[[paths]]
path = "$($dir -replace '\\', '/')"
alias = "$alias"
"@
    } else {
        @'

# Каждая папка с исходниками описывается своей секцией. Алиас передаётся
# в каждом вызове инструмента параметром repo.
#
# [[paths]]
# path = "C:/Repo1C"
# alias = "main"
# language = "bsl"
'@
    }

    @"
[daemon]
http_host = "127.0.0.1"
http_port = $DaemonPort
log_level = "info"
$pathsBlock
"@ | Set-Content -Path $configPath -Encoding UTF8

    Write-Note $configPath
}

# --- 5. Автозапуск ----------------------------------------------------------

$serveArgs = "serve --transport http --port $serverPort --config `"$configPath`""

if ($RegisterAutostart -and -not $UseScheduledTasks) {
    Write-Step 'Регистрирую автозапуск через папку автозагрузки'

    # Планировщик задач требует прав администратора даже для задачи от своего же
    # имени, поэтому обычный путь — автозагрузка. Запуск идёт через wscript,
    # иначе при каждом входе в систему всплывали бы два консольных окна.
    $starter = Join-Path $InstallDir 'start-hidden.vbs'

    # Файлы .vbs пишутся в кодировке системы: wscript читает их как ANSI, а не
    # как UTF-8, и в кодировке ASCII кириллица комментариев превращается в мусор.
    @"
' Скрытый запуск индексатора и сервера. Второй аргумент Run: 0 — окно не
' показывать, False — не ждать завершения.
Set sh = CreateObject("WScript.Shell")
sh.Run Chr(34) & "$exePath" & Chr(34) & " daemon run", 0, False
WScript.Sleep 3000
sh.Run Chr(34) & "$exePath" & Chr(34) & " serve --transport http --port $serverPort --config " & Chr(34) & "$configPath" & Chr(34), 0, False
"@ | Set-Content -Path $starter -Encoding Default

    $startupDir = [Environment]::GetFolderPath('Startup')
    $link = Join-Path $startupDir 'code-index.vbs'
    @"
' Ярлык автозагрузки: запускает скрытый запускатель из папки установки.
Set sh = CreateObject("WScript.Shell")
sh.Run Chr(34) & "$starter" & Chr(34), 0, False
"@ | Set-Content -Path $link -Encoding Default

    Write-Note "автозагрузка: $link"
    Write-Note "запустить сейчас, не выходя из системы — wscript `"$starter`""
}

if ($RegisterAutostart -and $UseScheduledTasks) {
    Write-Step 'Регистрирую автозапуск задачами планировщика'

    $tasks = @(
        @{ Name = "${TaskPrefix}Daemon"; Args = 'daemon run' },
        @{ Name = "${TaskPrefix}Serve";  Args = $serveArgs }
    )

    foreach ($task in $tasks) {
        & schtasks.exe /Query /TN $task.Name *> $null
        $exists = ($LASTEXITCODE -eq 0)

        if ($exists -and -not $ReplaceTasks) {
            Write-Warning "Задача $($task.Name) уже есть — оставляю как есть. Перезаписать: повторите с ключом -ReplaceTasks, либо задайте другой -TaskPrefix."
            continue
        }

        $cmd = "`"$exePath`" $($task.Args)"
        & schtasks.exe /Create /TN $task.Name /TR $cmd /SC ONLOGON /RL LIMITED /F | Out-Null
        if ($LASTEXITCODE -ne 0) {
            Write-Warning "Не удалось зарегистрировать задачу $($task.Name) (код $LASTEXITCODE). Планировщик требует прав администратора: запустите установщик из окна PowerShell от имени администратора либо повторите без ключа -UseScheduledTasks — тогда автозапуск пойдёт через папку автозагрузки и прав не потребует."
        } else {
            Write-Note "задача $($task.Name) $(if ($exists) { 'перезаписана' } else { 'создана' })"
        }
    }

    Write-Note "задачи стартуют при следующем входе в систему; запустить сейчас — schtasks /Run /TN ${TaskPrefix}Daemon"
}

# --- 6. Что дальше ----------------------------------------------------------

Write-Host ''
Write-Step 'Готово'
Write-Host ''
Write-Host 'Запуск вручную (два процесса — индексатор и сервер):' -ForegroundColor White
Write-Host "  `"$exePath`" daemon run"
Write-Host "  `"$exePath`" serve --transport http --port $serverPort --config `"$configPath`""
Write-Host ''
Write-Host 'Блок для .mcp.json вашего клиента:' -ForegroundColor White
Write-Host @"
  {
    "mcpServers": {
      "code-index": {
        "type": "http",
        "url": "http://127.0.0.1:$serverPort/mcp"
      }
    }
  }
"@
Write-Host ''
Write-Host "Папки с исходниками описываются в $configPath — по секции на папку." -ForegroundColor White
Write-Host 'Полное руководство: https://github.com/Regsorm/code-index-mcp#readme' -ForegroundColor White
