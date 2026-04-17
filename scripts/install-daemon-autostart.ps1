# Установка автозапуска фонового демона code-index через Scheduled Task (Windows).
#
# Скрипт создаёт задачу, которая при входе текущего пользователя запускает
# `code-index.exe daemon run` без консольного окна. Параметры триггера — только
# логин пользователя, без прав администратора.
#
# Запуск:
#   powershell -ExecutionPolicy Bypass -File scripts\install-daemon-autostart.ps1
#
# Параметры:
#   -BinaryPath   — путь к code-index.exe (по умолчанию: release-сборка рядом)
#   -TaskName     — имя задачи (по умолчанию CodeIndexDaemon)
#   -StartNow     — сразу запустить задачу после создания
#   -Uninstall    — удалить задачу вместо создания

param(
    [string]$BinaryPath    = "",
    [string]$CodeIndexHome = "",
    [string]$TaskName      = "CodeIndexDaemon",
    [switch]$StartNow,
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"

function Resolve-BinaryPath {
    param([string]$Given)
    if ($Given -ne "") {
        if (-not (Test-Path $Given)) {
            throw "Бинарник не найден: $Given"
        }
        return (Resolve-Path $Given).Path
    }
    # Значение по умолчанию: release-сборка рядом со скриптом.
    $scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
    $candidate = Join-Path $scriptDir "..\target\release\code-index.exe"
    if (-not (Test-Path $candidate)) {
        throw "Бинарник не найден: $candidate. Соберите release через 'cargo build --release' либо укажите -BinaryPath."
    }
    return (Resolve-Path $candidate).Path
}

if ($Uninstall) {
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Write-Host "Задача '$TaskName' удалена."
    } else {
        Write-Host "Задача '$TaskName' не найдена — ничего не делаем."
    }
    return
}

$exe = Resolve-BinaryPath -Given $BinaryPath
Write-Host "Бинарник: $exe"

# Если пользователь указал -CodeIndexHome — установить пользовательскую
# env var через setx и создать папку. Эта переменная определяет, где демон
# ищет daemon.toml и куда пишет pid/json/log. MCP-читатели и CLI-команды
# тоже используют её.
if ($CodeIndexHome -ne "") {
    if (-not (Test-Path $CodeIndexHome)) {
        New-Item -ItemType Directory -Path $CodeIndexHome -Force | Out-Null
        Write-Host "Создана папка: $CodeIndexHome"
    }
    # setx пишет в HKCU\Environment, новое значение видно в новых консолях.
    [System.Environment]::SetEnvironmentVariable("CODE_INDEX_HOME", $CodeIndexHome, "User")
    Write-Host "CODE_INDEX_HOME установлен (user-level): $CodeIndexHome"

    # Проверим, есть ли daemon.toml — если нет, создадим минимальный шаблон.
    $cfg = Join-Path $CodeIndexHome "daemon.toml"
    if (-not (Test-Path $cfg)) {
        @"
[daemon]
http_host = "127.0.0.1"
http_port = 0
log_level = "info"

# Перечислите отслеживаемые папки:
# [[paths]]
# path = "C:\\RepoUT"
"@ | Out-File -FilePath $cfg -Encoding UTF8
        Write-Host "Создан шаблон конфига: $cfg"
    }
}

# Если задача уже существует — удалим, чтобы пересоздать с актуальными параметрами.
if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    Write-Host "Существующая задача '$TaskName' удалена для пересоздания."
}

# Действие: запустить code-index.exe daemon run. Без окна консоли.
$action = New-ScheduledTaskAction -Execute $exe -Argument "daemon run"

# Триггер: при входе текущего пользователя.
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME

# Настройки: не стартовать если на батарее, рестартовать при падении, стабильный ID.
$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -StartWhenAvailable `
    -RestartCount 3 `
    -RestartInterval (New-TimeSpan -Minutes 1) `
    -ExecutionTimeLimit (New-TimeSpan -Hours 0) # 0 = без лимита

# Принципал: запускать от текущего пользователя без повышения.
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited

Register-ScheduledTask `
    -TaskName $TaskName `
    -Action $action `
    -Trigger $trigger `
    -Settings $settings `
    -Principal $principal `
    -Description "Фоновый демон code-index (индексация кодовых папок в режиме one-writer-many-readers)." | Out-Null

Write-Host "Задача '$TaskName' создана."
Write-Host "Триггер: при входе пользователя $env:USERNAME."

if ($StartNow) {
    # Запускаем демон напрямую (не через Scheduled Task), чтобы CODE_INDEX_HOME
    # из текущей сессии (или только что установленная через setx) точно попала
    # в процесс. Scheduled Task поймает её при следующем логине автоматически.
    $env:CODE_INDEX_HOME = if ($CodeIndexHome -ne "") { $CodeIndexHome } else { $env:CODE_INDEX_HOME }
    Start-Process -FilePath $exe -ArgumentList "daemon run" -WindowStyle Hidden
    Write-Host "Демон запущен прямо сейчас (PID см. в 'code-index daemon status')."
} else {
    Write-Host "Для немедленного запуска: Start-ScheduledTask -TaskName '$TaskName'"
    Write-Host "(либо перелогиньтесь — задача стартует при следующем входе пользователя)"
}

Write-Host ""
Write-Host "Проверка состояния демона:"
Write-Host "  $exe daemon status"
