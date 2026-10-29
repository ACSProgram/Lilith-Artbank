# 无窗口验收套件入口：后台运行、Idle 优先级、单测试线程、默认 cheap+default。
#
# 用法（在任意目录）：
#   powershell -ExecutionPolicy Bypass -File tools/acceptance/run.ps1
#   powershell -ExecutionPolicy Bypass -File tools/acceptance/run.ps1 -Tier heavy
#   powershell -ExecutionPolicy Bypass -File tools/acceptance/run.ps1 -Targets acceptance_library
#
# 不常驻、不注册计划任务；跑完即退出，不创建窗口。
param(
    [ValidateSet('cheap', 'default', 'heavy')]
    [string]$Tier = 'default',
    [string[]]$Targets = @(
        'acceptance_library',
        'acceptance_query',
        'acceptance_settings',
        'acceptance_logs',
        'acceptance_import',
        'acceptance_metadata',
        'acceptance_folders',
        'acceptance_history',
        'acceptance_viewer_edit',
        'acceptance_tasks',
        'acceptance_maintenance'
    )
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Set-Location $repo

# 子进程（cargo、测试与无头进程）继承本进程的 Idle 优先级。
try {
    [System.Diagnostics.Process]::GetCurrentProcess().PriorityClass = 'Idle'
    Write-Host 'priority: Idle'
}
catch {
    Write-Warning "无法设置 Idle 优先级：$($_.Exception.Message)"
}

$env:RAYON_NUM_THREADS = '2'
$env:LILITH_ACCEPT_TIERS = switch ($Tier) {
    'cheap' { 'cheap' }
    'default' { 'cheap,default' }
    'heavy' { 'cheap,default,heavy' }
}

$targetArgs = @()
foreach ($target in $Targets) {
    $targetArgs += @('--test', $target)
}
$cargoArgs = @(
    'test',
    '--manifest-path', 'src-tauri/Cargo.toml',
    '--features', 'headless'
) + $targetArgs + @('--', '--test-threads=1')

Write-Host "tiers: $env:LILITH_ACCEPT_TIERS"
Write-Host "targets: $($Targets -join ', ')"
# 报告是追加写的，汇总只看本轮的条目；容 5 秒时钟误差。
$startedAt = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() - 5000
& cargo @cargoArgs
$code = $LASTEXITCODE

# 报告路径跟随实际 target 目录，避免硬编码 src-tauri/target。
# cargo metadata 必须带 --manifest-path：仓库根没有 Cargo.toml，直接调用会报错退出。
$targetDir = (& cargo metadata --manifest-path 'src-tauri/Cargo.toml' --format-version 1 --no-deps | ConvertFrom-Json).target_directory
if (-not $targetDir) {
    Write-Warning "无法确定 target 目录，跳过报告汇总（测试退出码 $code）"
    exit $code
}
$report = Join-Path $targetDir 'acceptance/report.jsonl'
if (Test-Path $report) {
    Write-Host "report: $report"
    $failed = @()
    Get-Content $report | ForEach-Object {
        try {
            $entry = $_ | ConvertFrom-Json
            if ($entry.at -ge $startedAt -and -not $entry.ok) { $failed += $entry }
        }
        catch { }
    }
    if ($failed.Count -gt 0) {
        Write-Host "failed entries: $($failed.Count)"
        foreach ($entry in $failed) {
            Write-Host "  - $($entry.name) (tier=$($entry.tier), elapsedMs=$($entry.elapsedMs))"
        }
    }
    else {
        Write-Host 'failed entries: 0'
    }
}
exit $code
