$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# ============================================================
# easylife-ai local offline installer for Windows
# Installs from binaries in the same directory as this script.
# No network access required.
# ============================================================

function Write-ErrorAndExit {
    param([Parameter(Mandatory = $true)][string]$Message)
    Write-Host "Error: $Message" -ForegroundColor Red
    exit 1
}

function Write-Success {
    param([Parameter(Mandatory = $true)][string]$Message)
    Write-Host $Message -ForegroundColor Green
}

function Write-Warning {
    param([Parameter(Mandatory = $true)][string]$Message)
    Write-Host $Message -ForegroundColor Yellow
}

function Normalize-PathString {
    param([Parameter(Mandatory = $true)][string]$Path)
    try {
        return ([IO.Path]::GetFullPath($Path.Trim())).TrimEnd('\').ToLowerInvariant()
    } catch {
        return ($Path.Trim()).TrimEnd('\').ToLowerInvariant()
    }
}

function Test-FileAvailable {
    param([Parameter(Mandatory = $true)][string]$Path)
    try {
        $stream = [System.IO.File]::Open($Path, 'Open', 'Write', 'None')
        $stream.Close()
        return $true
    } catch {
        return $false
    }
}

function Stop-EasylifeAiBackgroundService {
    param(
        [Parameter(Mandatory = $true)][string]$EasylifeAiExe,
        [Parameter(Mandatory = $false)][switch]$Hard
    )
    if (-not (Test-Path -LiteralPath $EasylifeAiExe)) {
        return $false
    }
    $args = @('bg', 'shutdown')
    if ($Hard) {
        $args += '--hard'
    }
    try {
        & $EasylifeAiExe @args *> $null
        return $LASTEXITCODE -eq 0
    } catch {
        return $false
    }
}

function Get-EasylifeAiManagedProcesses {
    param([Parameter(Mandatory = $true)][string]$InstallDir)
    $targetPaths = @(
        (Join-Path $InstallDir 'easylife-ai.exe'),
        (Join-Path $InstallDir 'git.exe')
    )
    $normalizedTargets = $targetPaths | ForEach-Object { Normalize-PathString $_ }
    $processes = Get-Process -ErrorAction SilentlyContinue | Where-Object {
        try {
            $procPath = $_.Path
            if ([string]::IsNullOrWhiteSpace($procPath)) { return $false }
            $normalizedProc = Normalize-PathString $procPath
            return $normalizedTargets -contains $normalizedProc
        } catch {
            return $false
        }
    }
    return $processes
}

# ============================================================
# 用户可配置区 — 部署时按需修改以下变量
# ============================================================

# 安装目录：easylife-ai 二进制文件的安装位置
# 默认安装到当前用户 home 目录下的 .easylife-ai\bin
# 私有化部署或统一管理多用户时可改为绝对路径（如 C:\easylife-ai\bin）
$InstallDir = Join-Path $HOME '.easylife-ai\bin'

# 自动更新服务端地址：客户端检查新版本时访问的 URL
# 私有化部署时改为内部服务器地址（如 https://your-internal-server.com）
$UpdateReleaseUrl = 'https://github.com/easylife1997/easylife-ai/releases'

# 稳定版更新目标版本：每次发布时手动更新，格式为 x.y.z
$UpdateReleaseVersion = '1.0.6'

# 自动更新检查间隔（秒）：默认 86400 秒（24 小时）
# 设为更大的值可降低检查频率；设为 0 时客户端行为由 DisableAutoUpdates 控制
$UpdateCheckIntervalSeconds = 3600

# 更新通道：控制客户端跟踪哪个发布通道
# 可选值：latest（稳定版）、next（预览版）
$UpdateChannel = 'latest'

# 是否禁用自动更新：$false = 允许自动更新（默认）；$true = 锁定当前版本，不自动升级
# 注意：此值仅在用户配置文件中不存在该字段时写入，已有配置的用户不受影响
$DisableAutoUpdates = $false

# 是否禁用版本检查提示：$false = 正常显示版本过旧提示（默认）；$true = 静默跳过
# 注意：与 DisableAutoUpdates 相同，仅在该字段缺失时写入
$DisableVersionChecks = $false

# ============================================================

# Detect architecture
$arch = if ([Environment]::Is64BitOperatingSystem) {
    if ([Environment]::GetEnvironmentVariable('PROCESSOR_ARCHITECTURE') -eq 'ARM64') {
        'arm64'
    } else {
        'x64'
    }
} else {
    Write-ErrorAndExit "32-bit Windows is not supported"
}

$os = 'windows'

# Script directory (where binaries live)
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$binaryName = "easylife-ai-$os-$arch.exe"
$binaryPath = Join-Path $scriptDir $binaryName

if (-not (Test-Path -LiteralPath $binaryPath)) {
    Write-ErrorAndExit "Binary not found: $binaryPath`nMake sure $binaryName is in the same directory as this script."
}

Write-Host "Installing easylife-ai from $binaryPath..."

# Detect standard git (needed for git-og symlink and config)
$stdGitPath = $null
$gitCandidates = @(
    (Get-Command git -ErrorAction SilentlyContinue).Source,
    'C:\Program Files\Git\cmd\git.exe',
    'C:\Program Files (x86)\Git\cmd\git.exe',
    'C:\Program Files\Git\bin\git.exe',
    'C:\Program Files (x86)\Git\bin\git.exe'
)

foreach ($candidate in $gitCandidates) {
    if ([string]::IsNullOrWhiteSpace($candidate)) { continue }
    if ($candidate -like '*easylife-ai*') { continue }
    if ($candidate -like '*git-ai*') { continue }
    if (Test-Path -LiteralPath $candidate) {
        try {
            & $candidate --version *> $null
            if ($LASTEXITCODE -eq 0) {
                $stdGitPath = $candidate
                break
            }
        } catch {}
    }
}

if (-not $stdGitPath) {
    $configJson = Join-Path $HOME '.easylife-ai\config.json'
    if (Test-Path -LiteralPath $configJson) {
        try {
            $cfg = Get-Content -LiteralPath $configJson -Raw | ConvertFrom-Json
            if ($cfg.git_path -and ($cfg.git_path -notlike '*easylife-ai*') -and ($cfg.git_path -notlike '*git-ai*')) {
                if (Test-Path -LiteralPath $cfg.git_path) {
                    & $cfg.git_path --version *> $null
                    if ($LASTEXITCODE -eq 0) {
                        $stdGitPath = $cfg.git_path
                    }
                }
            }
        } catch {}
    }
}

if (-not $stdGitPath) {
    Write-ErrorAndExit "Could not detect a standard git binary on PATH. Please ensure Git is installed."
}

# Verify detected git is usable
try {
    & $stdGitPath --version | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'bad' }
} catch {
    Write-ErrorAndExit "Detected git at $stdGitPath is not usable (--version failed). Please ensure you have Git installed."
}

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

$finalExe = Join-Path $InstallDir 'easylife-ai.exe'
$gitShimExe = Join-Path $InstallDir 'git.exe'
$gitOgCmd = Join-Path $InstallDir 'git-og.cmd'

# Shutdown background service if running
if (Test-Path -LiteralPath $finalExe) {
    Write-Host 'Shutting down background service...'
    $shutdownOk = Stop-EasylifeAiBackgroundService -EasylifeAiExe $finalExe
    if (-not $shutdownOk) {
        Stop-EasylifeAiBackgroundService -EasylifeAiExe $finalExe -Hard | Out-Null
    }
    Start-Sleep -Milliseconds 500
}

# Kill any remaining processes
$remainingProcs = Get-EasylifeAiManagedProcesses -InstallDir $InstallDir
if ($remainingProcs) {
    Write-Host 'Stopping remaining processes...'
    $remainingProcs | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 500
}

# Wait for files to be available
$maxWait = 10
$waited = 0
while ($waited -lt $maxWait) {
    $allAvailable = $true
    foreach ($f in @($finalExe, $gitShimExe)) {
        if ((Test-Path -LiteralPath $f) -and (-not (Test-FileAvailable $f))) {
            $allAvailable = $false
            break
        }
    }
    if ($allAvailable) { break }
    Start-Sleep -Milliseconds 500
    $waited++
}

# Copy binary to easylife-ai.exe (primary)
Copy-Item -Force -Path $binaryPath -Destination $finalExe

# Create git.exe shim (copy of easylife-ai.exe)
Copy-Item -Force -Path $binaryPath -Destination $gitShimExe

# Create git-og.cmd shim that calls standard git
$gitOgContent = "@echo off`r`n`"$stdGitPath`" %*"
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText($gitOgCmd, $gitOgContent, $utf8NoBom)

# Unblock files (remove execution restrictions from downloaded binaries)
try { Unblock-File -Path $finalExe -ErrorAction SilentlyContinue } catch { }
try { Unblock-File -Path $gitShimExe -ErrorAction SilentlyContinue } catch { }

Write-Success "Installed to $InstallDir"

# Print installed version
try {
    $installedVersion = & $finalExe --version 2>&1
    Write-Host "Installed easylife-ai $installedVersion"
} catch {
    Write-Host "Installed easylife-ai (version unknown)"
}

# Initialize update configuration without overwriting user settings.
$configDir = Join-Path $HOME '.easylife-ai'
$configJsonPath = Join-Path $configDir 'config.json'
$tmpConfigJsonPath = "$configJsonPath.tmp.$PID"
New-Item -ItemType Directory -Force -Path $configDir | Out-Null

try {
    if (Test-Path -LiteralPath $configJsonPath) {
        $cfg = Get-Content -LiteralPath $configJsonPath -Raw | ConvertFrom-Json
        if ($null -eq $cfg -or $cfg -isnot [pscustomobject]) {
            throw "config root must be a JSON object"
        }
    } else {
        $cfg = [pscustomobject]@{
            git_path = $stdGitPath
            feature_flags = [pscustomobject]@{
                async_mode = $true
            }
        }
    }

    # Always overwrite these three fields regardless of existing values.
    $overwrite = [ordered]@{
        update_release_url = $UpdateReleaseUrl
        update_release_version = $UpdateReleaseVersion
        update_check_interval_seconds = $UpdateCheckIntervalSeconds
        update_channel = $UpdateChannel
    }
    foreach ($entry in $overwrite.GetEnumerator()) {
        if ($cfg.PSObject.Properties.Name -contains $entry.Key) {
            $cfg.($entry.Key) = $entry.Value
        } else {
            $cfg | Add-Member -NotePropertyName $entry.Key -NotePropertyValue $entry.Value
        }
    }
    # Only fill these if absent — user may have intentionally disabled updates.
    $fillIfAbsent = [ordered]@{
        disable_auto_updates = $DisableAutoUpdates
        disable_version_checks = $DisableVersionChecks
    }
    foreach ($entry in $fillIfAbsent.GetEnumerator()) {
        if (-not ($cfg.PSObject.Properties.Name -contains $entry.Key)) {
            $cfg | Add-Member -NotePropertyName $entry.Key -NotePropertyValue $entry.Value
        }
    }

    $cfg | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $tmpConfigJsonPath -Encoding UTF8
    $jsonText = [System.IO.File]::ReadAllText($tmpConfigJsonPath)
    [System.IO.File]::WriteAllText($configJsonPath, $jsonText, $utf8NoBom)
    Remove-Item -LiteralPath $tmpConfigJsonPath -Force -ErrorAction SilentlyContinue
} catch {
    Remove-Item -LiteralPath $tmpConfigJsonPath -Force -ErrorAction SilentlyContinue
    Write-Warning "Failed to update config.json: $($_.Exception.Message)"
}

# Write tracker-config.json if TRACKER_URL + TEAM_ID + TEAM_KEY are provided
$trackerConfigPath = Join-Path $configDir 'tracker-config.json'
if ($env:TRACKER_URL -and $env:TEAM_ID -and $env:TEAM_KEY) {
    # Load existing blacklist if config already exists
    $existingBlacklist = @()
    if (Test-Path -LiteralPath $trackerConfigPath) {
        try {
            $existingConfig = Get-Content -LiteralPath $trackerConfigPath -Raw | ConvertFrom-Json
            if ($existingConfig.blacklist) {
                $existingBlacklist = $existingConfig.blacklist
            }
        } catch {}
    }

    # USER_NAME is the sole source of tracker identity. Do not fall back
    # to USERNAME, GIT_AI_USERNAME, or git user.email.
    $installUsername = $env:USER_NAME
    if ([string]::IsNullOrWhiteSpace($installUsername)) {
        Write-Warning "USER_NAME is missing or blank; tracker configuration was not written."
        $installUsername = $null
    }

    if ($installUsername) {
        Write-Host "Configuring tracker with username: $installUsername (from USER_NAME)"
        $trackerConfig = @{
            tracker_url = $env:TRACKER_URL
            team_id     = $env:TEAM_ID
            team_key    = $env:TEAM_KEY
            username    = $installUsername
            blacklist   = $existingBlacklist
        }
        $trackerJson = $trackerConfig | ConvertTo-Json -Depth 3 -Compress
        [System.IO.File]::WriteAllText($trackerConfigPath, $trackerJson, $utf8NoBom)
        Write-Success "Tracker config written to $trackerConfigPath"
    }
} else {
    Write-Host 'Tracker config skipped (set TRACKER_URL, TEAM_ID, TEAM_KEY to enable)'
}

# Install hooks
Write-Host 'Setting up IDE/agent hooks...'
try {
    & $finalExe install-hooks | Out-Host
    if ($LASTEXITCODE -eq 0) {
        Write-Success 'IDE/agent hooks configured'
    } else {
        Write-Warning 'Failed to set up IDE/agent hooks. Run "easylife-ai install-hooks" manually.'
    }
} catch {
    Write-Warning 'Failed to set up IDE/agent hooks. Run "easylife-ai install-hooks" manually.'
}

# Update PATH (User scope)
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$pathEntries = if ($userPath) { $userPath.Split(';') } else { @() }
$normalizedEntries = $pathEntries | ForEach-Object { Normalize-PathString $_ }
$normalizedInstallDir = Normalize-PathString $InstallDir

if ($normalizedEntries -notcontains $normalizedInstallDir) {
    $newPath = if ($userPath) { "$InstallDir;$userPath" } else { $InstallDir }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    $env:Path = "$InstallDir;$env:Path"
    Write-Success "Added $InstallDir to User PATH"
} else {
    Write-Success "PATH already contains $InstallDir"
}

# Configure Git Bash if present
$gitBashConfigured = $false
$gitBashAlreadyConfigured = $false

$bashrcPath = Join-Path $HOME '.bashrc'
$bashProfilePath = Join-Path $HOME '.bash_profile'

$gitBashInstalled = $false
$gitForWindowsPaths = @()
if ($env:ProgramFiles) { $gitForWindowsPaths += Join-Path $env:ProgramFiles 'Git\bin\bash.exe' }
if (${env:ProgramFiles(x86)}) { $gitForWindowsPaths += Join-Path ${env:ProgramFiles(x86)} 'Git\bin\bash.exe' }
if ($env:LOCALAPPDATA) { $gitForWindowsPaths += Join-Path $env:LOCALAPPDATA 'Programs\Git\bin\bash.exe' }
foreach ($p in $gitForWindowsPaths) {
    if ($p -and (Test-Path -LiteralPath $p)) {
        $gitBashInstalled = $true
        break
    }
}

if ($gitBashInstalled) {
    $targetBashConfig = $null
    if (Test-Path -LiteralPath $bashrcPath) {
        $targetBashConfig = $bashrcPath
    } elseif (Test-Path -LiteralPath $bashProfilePath) {
        $targetBashConfig = $bashProfilePath
    } else {
        $targetBashConfig = $bashrcPath
    }

    $pathLine = "export PATH=`"$($InstallDir -replace '\\', '/'):`$PATH`""
    $markerString = '.easylife-ai/bin'
    
    $alreadyPresent = $false
    if (Test-Path -LiteralPath $targetBashConfig) {
        $content = Get-Content -LiteralPath $targetBashConfig -Raw -ErrorAction SilentlyContinue
        if ($content -and $content.Contains($markerString)) {
            $alreadyPresent = $true
        }
    }

    if ($alreadyPresent) {
        $gitBashAlreadyConfigured = $true
    } else {
        $timestamp = Get-Date -Format 'yyyy-MM-dd HH:mm:ss'
        $appendContent = "`n# Added by easylife-ai installer on $timestamp`n$pathLine`n"
        [System.IO.File]::AppendAllText($targetBashConfig, $appendContent, $utf8NoBom)
        $gitBashConfigured = $true
    }
}

if ($gitBashConfigured) {
    Write-Success "Successfully configured Git Bash ($targetBashConfig)"
} elseif ($gitBashAlreadyConfigured) {
    Write-Success "Git Bash already configured ($targetBashConfig)"
}

Write-Host ''
Write-Host 'Installation complete!' -ForegroundColor Green
Write-Host 'Close and reopen your terminal and IDE sessions to use easylife-ai.' -ForegroundColor Yellow
Write-Host ''
Write-Host 'You can now run:' -ForegroundColor Cyan
Write-Host '  easylife-ai --version'
Write-Host '  git --version'
Write-Host '  git-og --version (standard git)'