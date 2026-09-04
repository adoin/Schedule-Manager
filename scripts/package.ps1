param(
    [switch]$Installer,
    [switch]$SkipBuild,
    [string]$Version = ""
)

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$stagePath = Join-Path $projectRoot 'dist\ScheduleManager'
$binaryPath = Join-Path $projectRoot 'target\release\schedule-manager.exe'
$widgetBinaryPath = Join-Path $projectRoot 'target\release\schedule-desktop-widget.exe'
if ([string]::IsNullOrWhiteSpace($Version)) {
    $manifest = Get-Content -LiteralPath (Join-Path $projectRoot 'Cargo.toml') -Raw
    $match = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"')
    if (-not $match.Success) {
        throw 'Unable to read package version from Cargo.toml.'
    }
    $Version = $match.Groups[1].Value
}

# openssl-src needs a complete Perl distribution when producing the static TLS
# library. Prefer Strawberry Perl over the reduced Perl bundled with Git.
$strawberryPerl = 'C:\Strawberry\perl\bin\perl.exe'
if (Test-Path -LiteralPath $strawberryPerl) {
    $env:Path = 'C:\Strawberry\perl\bin;C:\Strawberry\c\bin;' + $env:Path
}
if (-not (Get-Command perl.exe -ErrorAction SilentlyContinue)) {
    throw 'Perl is required to build static OpenSSL. Install Strawberry Perl first.'
}

Push-Location $projectRoot
try {
    if (-not $SkipBuild) {
        cargo build --release --locked --bins --timings
        if ($LASTEXITCODE -ne 0) {
            throw "Cargo release build failed with exit code $LASTEXITCODE."
        }
    }
    if (-not (Test-Path -LiteralPath $binaryPath)) {
        throw "Release executable missing: $binaryPath"
    }
    if (-not (Test-Path -LiteralPath $widgetBinaryPath)) {
        throw "Desktop widget executable missing: $widgetBinaryPath"
    }
    if (Test-Path -LiteralPath $stagePath) {
        $resolvedStage = (Resolve-Path -LiteralPath $stagePath).Path
        $resolvedRoot = (Resolve-Path -LiteralPath $projectRoot).Path
        if (-not $resolvedStage.StartsWith($resolvedRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing to remove path outside project: $resolvedStage"
        }
        Remove-Item -LiteralPath $resolvedStage -Recurse -Force
    }
    New-Item -ItemType Directory -Path $stagePath -Force | Out-Null
    Copy-Item -LiteralPath $binaryPath -Destination (Join-Path $stagePath 'ScheduleManager.exe')
    Copy-Item -LiteralPath $widgetBinaryPath -Destination (Join-Path $stagePath 'schedule-desktop-widget.exe')
    Copy-Item -LiteralPath (Join-Path $projectRoot 'README.md') -Destination (Join-Path $stagePath 'README.md')
    Copy-Item -LiteralPath (Join-Path $projectRoot 'scripts\collect-widget-diagnostics.ps1') -Destination (Join-Path $stagePath 'collect-widget-diagnostics.ps1')
    Write-Host "Portable folder ready: $stagePath"

    if ($Installer) {
        $compiler = Get-Command ISCC.exe -ErrorAction SilentlyContinue
        $compilerPath = if ($compiler) { $compiler.Source } else { $null }
        if (-not $compiler) {
            $innoCandidates = @(
                (Join-Path ([Environment]::GetFolderPath('ProgramFilesX86')) 'Inno Setup 6\ISCC.exe'),
                (Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Inno Setup 6\ISCC.exe')
            )
            $compilerPath = $innoCandidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
            if (-not $compilerPath) {
                throw 'Inno Setup compiler (ISCC.exe) not found.'
            }
        }
        $installerOutput = (New-Item -ItemType Directory -Path (Join-Path $projectRoot 'dist') -Force).FullName
        & $compilerPath `
            (Join-Path $projectRoot 'installer\ScheduleManager.iss') `
            "/DAppVersion=$Version" `
            "/DSourceDir=$stagePath" `
            "/DOutputDir=$installerOutput"
        if ($LASTEXITCODE -ne 0) {
            throw "Inno Setup failed with exit code $LASTEXITCODE."
        }
    }
}
finally {
    Pop-Location
}
