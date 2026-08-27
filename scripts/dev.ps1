param(
    [int]$DebounceMs = 700,
    [int]$CrashRestartLimit = 3,
    [int]$CrashRestartWindowSeconds = 30,
    [int]$CrashRestartDelayMs = 750,
    [string]$ApiBaseUrl = "",
    [switch]$KeepExisting,
    [switch]$NoLaunch
)

$ErrorActionPreference = "Stop"

$scriptPath = $MyInvocation.MyCommand.Path
$scriptDir = Split-Path -Parent $scriptPath
$root = Split-Path -Parent $scriptDir
Set-Location $root

$stateDir = Join-Path $root "target\dev-watch"
$pidFile = Join-Path $stateDir "runner.pid"
$logDate = Get-Date -Format "yyyy-MM-dd"
$stdoutLog = Join-Path $stateDir "dev-watch.$logDate.out.log"
$stderrLog = Join-Path $stateDir "dev-watch.$logDate.err.log"
$mainExe = Join-Path $root "target\debug\schedule-manager.exe"
$widgetExe = Join-Path $root "target\debug\schedule-desktop-widget.exe"
$exitSentinel = Join-Path $stateDir "intentional-exit"

New-Item -ItemType Directory -Force -Path $stateDir | Out-Null
Remove-Item -LiteralPath $exitSentinel -Force -ErrorAction SilentlyContinue
$env:SCHEDULE_WATCHER_EXIT_SENTINEL = $exitSentinel

# openssl-src needs a complete Perl distribution on a clean Windows build.
$strawberryPerl = "C:\Strawberry\perl\bin\perl.exe"
if (Test-Path -LiteralPath $strawberryPerl) {
    $env:Path = "C:\Strawberry\perl\bin;C:\Strawberry\c\bin;$env:Path"
}

if ($ApiBaseUrl) {
    $env:SCHEDULE_API_BASE_URL = $ApiBaseUrl
}

function Write-DevLog {
    param(
        [string]$Message,
        [ValidateSet("INFO", "WARN", "ERROR")]
        [string]$Level = "INFO"
    )

    $line = "[dev] $(Get-Date -Format o) [$Level] $Message"
    Write-Host $line
    $line | Add-Content -LiteralPath $stdoutLog
    if ($Level -eq "ERROR") {
        $line | Add-Content -LiteralPath $stderrLog
    }
}

function Stop-ProcessTree {
    param([int]$ProcessId)

    $children = Get-CimInstance Win32_Process -Filter "ParentProcessId=$ProcessId" -ErrorAction SilentlyContinue
    foreach ($child in $children) {
        Stop-ProcessTree -ProcessId $child.ProcessId
    }

    if (Get-Process -Id $ProcessId -ErrorAction SilentlyContinue) {
        Stop-Process -Id $ProcessId -Force -ErrorAction SilentlyContinue
    }
}

function Stop-ExistingRunner {
    if ($KeepExisting -or -not (Test-Path -LiteralPath $pidFile)) {
        return
    }

    $oldPidText = Get-Content -LiteralPath $pidFile -ErrorAction SilentlyContinue | Select-Object -First 1
    [int]$oldPid = 0
    if ([int]::TryParse($oldPidText, [ref]$oldPid) -and $oldPid -gt 0 -and $oldPid -ne $PID) {
        $oldRunner = Get-CimInstance Win32_Process -Filter "ProcessId=$oldPid" -ErrorAction SilentlyContinue
        $expectedScript = [regex]::Escape($scriptPath)
        $relativeScript = '(?i)(?:^|[\\/])scripts[\\/]dev\.ps1(?:\s|$)'
        $isDevRunner = $oldRunner -and (
            $oldRunner.CommandLine -match $expectedScript -or
            $oldRunner.CommandLine -match $relativeScript
        )
        if ($isDevRunner) {
            Write-DevLog "stop old dev runner pid=$oldPid"
            Stop-ProcessTree -ProcessId $oldPid
        }
        elseif ($oldRunner) {
            Write-DevLog "ignore stale pid file; pid=$oldPid belongs to another process" -Level "WARN"
        }
    }

    Remove-Item -LiteralPath $pidFile -Force -ErrorAction SilentlyContinue
}

function Stop-MainWindow {
    if ($script:mainProcess -and -not $script:mainProcess.HasExited) {
        Write-DevLog "stop schedule-manager pid=$($script:mainProcess.Id)"
        Stop-ProcessTree -ProcessId $script:mainProcess.Id
    }
    $script:mainProcess = $null

    # A previous watcher or a manual Debug launch must not survive a hot
    # restart. Only match this project's target\debug executable so packaged
    # or installed copies remain untouched.
    Get-CimInstance Win32_Process -Filter "Name='schedule-manager.exe'" -ErrorAction SilentlyContinue |
        Where-Object {
            $_.ExecutablePath -and
            [string]::Equals($_.ExecutablePath, $mainExe, [System.StringComparison]::OrdinalIgnoreCase)
        } |
        ForEach-Object {
            Write-DevLog "stop stray debug app pid=$($_.ProcessId)"
            Stop-ProcessTree -ProcessId $_.ProcessId
        }

    Get-CimInstance Win32_Process -Filter "Name='schedule-desktop-widget.exe'" -ErrorAction SilentlyContinue |
        Where-Object {
            $_.ExecutablePath -and
            [string]::Equals($_.ExecutablePath, $widgetExe, [System.StringComparison]::OrdinalIgnoreCase)
        } |
        ForEach-Object {
            Write-DevLog "stop stray debug widget pid=$($_.ProcessId)"
            Stop-ProcessTree -ProcessId $_.ProcessId
        }
}

function Build-App {
    Write-DevLog "cargo build --bins"
    Push-Location $root
    $previousErrorActionPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = "Continue"
        & cargo build --bins >> $stdoutLog 2>> $stderrLog
        $buildExitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousErrorActionPreference
        Pop-Location
    }

    if ($buildExitCode -ne 0) {
        Write-DevLog "build failed: exit $buildExitCode; fix the error and save again" -Level "ERROR"
        return $false
    }

    Write-DevLog "built app=$mainExe widget=$widgetExe"
    return $true
}

function Start-MainWindow {
    if ($NoLaunch) {
        Write-DevLog "NoLaunch enabled; build completed without starting the app"
        return $null
    }
    if (-not (Test-Path -LiteralPath $mainExe)) {
        Write-DevLog "app exe missing; skip start" -Level "WARN"
        return $null
    }

    Write-DevLog "start $mainExe"
    $process = Start-Process -FilePath $mainExe -WorkingDirectory $root -PassThru
    Write-DevLog "started schedule-manager pid=$($process.Id)"
    return $process
}

function Test-MainWindowExit {
    if (-not $script:mainProcess) {
        return
    }

    $processId = $script:mainProcess.Id
    $script:mainProcess.Refresh()
    if (-not $script:mainProcess.HasExited) {
        return
    }

    $exitCode = $script:mainProcess.ExitCode
    $level = if ($exitCode -eq 0) { "INFO" } else { "ERROR" }
    Write-DevLog "schedule-manager exited pid=$processId exit=$exitCode" -Level $level
    $script:mainProcess = $null
    if (Test-Path -LiteralPath $exitSentinel) {
        Remove-Item -LiteralPath $exitSentinel -Force -ErrorAction SilentlyContinue
        Write-DevLog "intentional app exit; watcher remains active without relaunching"
        return
    }
    $now = Get-Date
    $script:crashRestartTimes = @(
        $script:crashRestartTimes | Where-Object {
            ($now - $_).TotalSeconds -lt $CrashRestartWindowSeconds
        }
    )
    if ($script:crashRestartTimes.Count -ge $CrashRestartLimit) {
        Write-DevLog "crash restart suppressed after $CrashRestartLimit failures in ${CrashRestartWindowSeconds}s" -Level "ERROR"
        return
    }

    $script:crashRestartTimes += $now
    $attempt = $script:crashRestartTimes.Count
    Write-DevLog "restart app after unexpected exit attempt=$attempt/$CrashRestartLimit delay_ms=$CrashRestartDelayMs" -Level "WARN"
    Start-Sleep -Milliseconds $CrashRestartDelayMs
    $script:mainProcess = Start-MainWindow
}

function Restart-DevApp {
    $script:crashRestartTimes = @()
    Stop-MainWindow
    if (Build-App) {
        $script:mainProcess = Start-MainWindow
    }
}

Stop-ExistingRunner
Set-Content -LiteralPath $pidFile -Value $PID

$watcher = New-Object System.IO.FileSystemWatcher
$watcher.Path = $root
$watcher.IncludeSubdirectories = $true
$watcher.EnableRaisingEvents = $true
$watcher.Filter = "*.*"

$lastRestart = Get-Date "2000-01-01"
$script:mainProcess = $null
$script:crashRestartTimes = @()

try {
    Restart-DevApp
    # Treat the initial build as the latest restart. Cargo may update Cargo.lock
    # during a build; that generated write must not immediately stop the app we
    # just launched.
    $lastRestart = Get-Date
    Write-Host "[dev] watching src/, ui/, assets/, build.rs and Cargo.toml. Ctrl+C to stop." -ForegroundColor Green
    Write-Host "[dev] logs: $stdoutLog ; $stderrLog" -ForegroundColor DarkGray

    while ($true) {
        $change = $watcher.WaitForChanged("Changed, Created, Deleted, Renamed", 1000)
        if ($change.TimedOut) {
            Test-MainWindowExit
            continue
        }

        $path = $change.Name -replace "/", "\"
        $isWatched =
            $path -like "src\*" -or
            $path -like "ui\*" -or
            $path -like "assets\*" -or
            $path -eq "build.rs" -or
            $path -eq "Cargo.toml"

        if (-not $isWatched) {
            continue
        }

        $now = Get-Date
        if (($now - $lastRestart).TotalMilliseconds -lt $DebounceMs) {
            continue
        }

        Write-Host "[dev] change: $path" -ForegroundColor DarkGray
        Restart-DevApp
        # Timestamp completion rather than start. Otherwise events queued while
        # Cargo is compiling look old enough to bypass the debounce and cause a
        # stop/build/start loop.
        $lastRestart = Get-Date
    }
}
finally {
    $watcher.Dispose()
    Stop-MainWindow

    $currentPidText = Get-Content -LiteralPath $pidFile -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($currentPidText -eq "$PID") {
        Remove-Item -LiteralPath $pidFile -Force -ErrorAction SilentlyContinue
    }
}
