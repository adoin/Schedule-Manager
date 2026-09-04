param(
    [string]$WidgetExecutable = "",
    [string]$OutputDirectory = ""
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$dataDirectory = Join-Path $env:LOCALAPPDATA 'Emssion\ScheduleManager\data'
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $dataDirectory ("diagnostics\" + (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
}
$output = (New-Item -ItemType Directory -Path $OutputDirectory -Force).FullName
$processes = @(Get-CimInstance Win32_Process | Where-Object {
    $_.Name -in @('schedule-desktop-widget.exe', 'ScheduleManager.exe', 'schedule-manager.exe', 'explorer.exe', 'dwm.exe')
})
$processes | Select-Object Name, ProcessId, ParentProcessId, ExecutablePath, CreationDate |
    ConvertTo-Json -Depth 3 | Set-Content -LiteralPath (Join-Path $output 'processes.json') -Encoding utf8
Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, BuildNumber, LastBootUpTime |
    ConvertTo-Json | Set-Content -LiteralPath (Join-Path $output 'windows.json') -Encoding utf8
Get-CimInstance Win32_VideoController | Select-Object Name, DriverVersion, DriverDate, CurrentHorizontalResolution, CurrentVerticalResolution |
    ConvertTo-Json | Set-Content -LiteralPath (Join-Path $output 'graphics.json') -Encoding utf8

# Signal the new watchdog even if the UI is blocked; also works for a white
# surface whose UI heartbeat is still healthy. It records native window state.
New-Item -ItemType Directory -Path $dataDirectory -Force | Out-Null
Set-Content -LiteralPath (Join-Path $dataDirectory 'widget-diagnostic.signal') -Value 'manual'

# Use a freshly built helper, which can capture an older installed process too.
if ([string]::IsNullOrWhiteSpace($WidgetExecutable)) {
    $candidates = @(
        (Join-Path $projectRoot 'target\diagnostic\schedule-desktop-widget.exe'),
        (Join-Path $projectRoot 'target\debug\schedule-desktop-widget.exe'),
        (Join-Path $projectRoot 'target\release\schedule-desktop-widget.exe'),
        (Join-Path $PSScriptRoot 'schedule-desktop-widget.exe')
    )
    $WidgetExecutable = $candidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
}
if ($WidgetExecutable) {
    foreach ($process in ($processes | Where-Object { $_.Name -like '*schedule*' })) {
        $dumpPath = Join-Path $output ("{0}-{1}.dmp" -f $process.Name, $process.ProcessId)
        $startInfo = [Diagnostics.ProcessStartInfo]::new()
        $startInfo.FileName = $WidgetExecutable
        $startInfo.UseShellExecute = $false
        $startInfo.CreateNoWindow = $true
        $startInfo.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden
        foreach ($argument in @('--capture-dump', [string]$process.ProcessId, $dumpPath)) {
            $startInfo.ArgumentList.Add($argument)
        }
        $helper = [Diagnostics.Process]::Start($startInfo)
        try {
            if (-not $helper.WaitForExit(10000)) {
                $helper.Kill()
                Write-Warning "Dump helper timed out for PID $($process.ProcessId)."
            }
            elseif ($helper.ExitCode -ne 0 -or -not (Test-Path -LiteralPath $dumpPath)) {
                Write-Warning "No dump captured for PID $($process.ProcessId). Use the updated widget executable."
            }
        }
        finally { $helper.Dispose() }
    }
}
else { Write-Warning 'Build the updated widget first to enable thread dumps.' }

foreach ($logName in @('Application', 'System')) {
    $events = Get-WinEvent -FilterHashtable @{ LogName = $logName; StartTime = (Get-Date).AddDays(-2); Level = 1, 2, 3 } -ErrorAction SilentlyContinue |
        Where-Object { $_.ProviderName -match 'Application Error|Application Hang|Windows Error Reporting|Display|Dwm|Kernel-Power' -or $_.Message -match 'schedule-desktop-widget|ScheduleManager' } |
        Select-Object -First 100 TimeCreated, Id, ProviderName, Message
    $events | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $output "$logName-events.json") -Encoding utf8
}
$logDirectory = Join-Path $dataDirectory 'logs'
if (Test-Path -LiteralPath $logDirectory) {
    $copiedLogs = New-Item -ItemType Directory -Path (Join-Path $output 'logs') -Force
    Get-ChildItem -LiteralPath $logDirectory -File -Filter '*.log' |
        Where-Object { $_.LastWriteTime -ge (Get-Date).AddDays(-2) } |
        Copy-Item -Destination $copiedLogs.FullName
}
Write-Host "Diagnostics saved: $output"
