#Requires -Version 7.0
<#
.SYNOPSIS
    Kytrix ui_core 三平台验证流水线（Windows 本机侧入口）。

.DESCRIPTION
    本机（Windows）步骤：
        cargo check  -p ui_core --all-targets --all-features
        cargo +nightly clippy -p ui_core --all-targets --all-features -- -D warnings
        cargo test   -p ui_core --all-features
        cargo +nightly fmt -p ui_core -- --check          （只检查，不改文件）

    远端（macOS / Linux 真机）步骤：
        1. `git archive HEAD` 生成源码快照（**不含** target/ 与 .git/）；
        2. scp 到远端 /tmp，解包到 $HOME/kytrix-review；
        3. 在远端跑 `scripts/verify.sh`（check / clippy / test / fmt[-bench]），
           CARGO_TARGET_DIR=$HOME/kytrix-review-target（/tmp 可能是 tmpfs）；
        4. 日志落到 .review/remote-<platform>.log，结果用 TSV 汇总回本机；
        5. 默认删除远端源码目录（-KeepRemote 保留）。

    幂等：远端源码目录每次重建、target 目录复用（增量编译），可重复运行。
    任一平台任一步失败 ⇒ 退出码 1。

.PARAMETER Platform
    all（默认）= 本机 + mac + linux；mac / linux = 本机 + 指定远端。

.PARAMETER SkipRemote
    只跑本机步骤（远端 SSH 不可用时的降级路径）。

.PARAMETER Bench
    额外编译基准：本机跑 `cargo bench -p ui_core --features test-support --bench perf`
    （完整测量），远端跑 `--no-run`（只验证可编译）。

.PARAMETER KeepRemote
    保留远端 $HOME/kytrix-review 源码目录（默认删除；target 目录始终保留）。

.EXAMPLE
    pwsh scripts/verify.ps1
    pwsh scripts/verify.ps1 -Platform linux -Bench
    pwsh scripts/verify.ps1 -SkipRemote -Bench
#>
[CmdletBinding()]
param(
    [ValidateSet('all', 'mac', 'linux')]
    [string]$Platform = 'all',

    [switch]$SkipRemote,

    [switch]$Bench,

    [switch]$KeepRemote
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# 原生命令非零退出码不抛异常（由脚本自己判定）。
if (Test-Path variable:PSNativeCommandUseErrorActionPreference) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$RepoRoot = Split-Path -Parent $PSScriptRoot
$ReviewDir = Join-Path $RepoRoot '.review'
# 远端路径：单引号 = 字面量，交给远端 shell 展开 $HOME。
$RemoteRoot = '$HOME/kytrix-review'
$RemoteTargetDir = '$HOME/kytrix-review-target'
$RemoteArchive = 'kytrix-review-src.tar.gz'
$Hosts = [ordered]@{
    mac   = 'chenbaiyu@192.168.1.6'
    linux = 'chenbaiyu@192.168.130.128'
}

$script:Results = [System.Collections.Generic.List[object]]::new()

function Add-Result {
    param(
        [string]$PlatformName,
        [string]$Step,
        [ValidateSet('pass', 'fail', 'skip')]
        [string]$Status,
        [double]$Seconds
    )
    $script:Results.Add([pscustomobject]@{
            Platform = $PlatformName
            Step     = $Step
            Status   = $Status
            Seconds  = [math]::Round($Seconds, 1)
        })
}

function Get-Mark {
    param([string]$Status)
    switch ($Status) {
        'pass' { '✅' }
        'fail' { '❌' }
        default { '⏭' }
    }
}

function Write-StepResult {
    param([string]$PlatformName, [string]$Step, [string]$Status, [double]$Seconds, [string]$Detail = '')
    $mark = Get-Mark $Status
    $text = '{0} [{1}] {2} ({3:N1}s){4}' -f $mark, $PlatformName, $Step, $Seconds, $Detail
    $color = switch ($Status) {
        'pass' { 'Green' }
        'fail' { 'Red' }
        default { 'Yellow' }
    }
    Write-Host $text -ForegroundColor $color
    Add-Result -PlatformName $PlatformName -Step $Step -Status $Status -Seconds $Seconds
}

function Invoke-LocalStep {
    param([string]$Name, [string[]]$CargoArgs)
    $log = Join-Path $ReviewDir ("win-{0}.log" -f $Name)
    Write-Host ("---- [win] {0}: cargo {1}" -f $Name, ($CargoArgs -join ' ')) -ForegroundColor Cyan
    $started = Get-Date
    & cargo @CargoArgs 2>&1 | Tee-Object -FilePath $log | Out-Host
    $code = $LASTEXITCODE
    $seconds = ((Get-Date) - $started).TotalSeconds
    if ($code -eq 0) {
        Write-StepResult -PlatformName 'win' -Step $Name -Status 'pass' -Seconds $seconds
    }
    else {
        Write-StepResult -PlatformName 'win' -Step $Name -Status 'fail' -Seconds $seconds -Detail (" (exit {0})" -f $code)
    }
}

# ── 前置检查 ────────────────────────────────────────────────────────────────
if (-not (Test-Path $ReviewDir)) {
    New-Item -ItemType Directory -Path $ReviewDir -Force | Out-Null
}
Set-Location $RepoRoot

$commit = (& git -C $RepoRoot rev-parse --short HEAD 2>&1 | Select-Object -First 1)
$branch = (& git -C $RepoRoot rev-parse --abbrev-ref HEAD 2>&1 | Select-Object -First 1)
Write-Host "== kytrix verify: branch=$branch commit=$commit root=$RepoRoot" -ForegroundColor Cyan
Write-Host "== platform=$Platform skipRemote=$($SkipRemote.IsPresent) bench=$($Bench.IsPresent) keepRemote=$($KeepRemote.IsPresent)" -ForegroundColor Cyan

# ── 本机步骤 ────────────────────────────────────────────────────────────────
Invoke-LocalStep -Name 'check' -CargoArgs @('check', '-p', 'ui_core', '--all-targets', '--all-features')
Invoke-LocalStep -Name 'clippy' -CargoArgs @('+nightly', 'clippy', '-p', 'ui_core', '--all-targets', '--all-features', '--', '-D', 'warnings')
Invoke-LocalStep -Name 'test' -CargoArgs @('test', '-p', 'ui_core', '--all-features')
# fmt 只检查：`--check` 不会改任何文件（本仓库禁止顺手格式化别人正在编辑的文件）。
Invoke-LocalStep -Name 'fmt' -CargoArgs @('+nightly', 'fmt', '-p', 'ui_core', '--', '--check')
if ($Bench) {
    Invoke-LocalStep -Name 'bench' -CargoArgs @('bench', '-p', 'ui_core', '--features', 'test-support', '--bench', 'perf')
}

# ── 远端步骤 ────────────────────────────────────────────────────────────────
if ($SkipRemote) {
    Write-Host '== remote steps skipped (-SkipRemote)' -ForegroundColor Yellow
}
else {
    $targets = switch ($Platform) {
        'all' { @('mac', 'linux') }
        default { @($Platform) }
    }

    # 远端快照来自 HEAD：未提交的改动不会被带过去（先 commit 再验证）。
    $dirty = & git -C $RepoRoot status --porcelain -- crates/ui_core scripts 2>&1
    if ($dirty) {
        Write-Warning '远端快照来自 HEAD：crates/ui_core 或 scripts/ 有未提交改动，远端验证不会覆盖它们（先 commit）。'
    }

    foreach ($name in $targets) {
        $target = $Hosts[$name]
        $platformName = $name
        Write-Host ''
        Write-Host ("== remote {0} ({1})" -f $name, $target) -ForegroundColor Cyan

        # 1) 连通性（BatchMode：不会卡在密码提示上）。
        & ssh -o BatchMode=yes -o ConnectTimeout=10 $target 'true' 2>&1 | Out-Null
        if ($LASTEXITCODE -ne 0) {
            Write-StepResult -PlatformName $platformName -Step 'reachable' -Status 'fail' -Seconds 0 -Detail ' (ssh failed)'
            continue
        }

        # 2) 源码快照（git archive HEAD：不含 target/ 与 .git/）。
        $archive = Join-Path ([System.IO.Path]::GetTempPath()) ("kytrix-review-{0}.tar.gz" -f $name)
        if (Test-Path $archive) {
            Remove-Item -LiteralPath $archive -Force
        }
        & git -C $RepoRoot archive --format=tar.gz --output=$archive HEAD 2>&1 | Out-Null
        if ($LASTEXITCODE -ne 0 -or -not (Test-Path $archive)) {
            Write-StepResult -PlatformName $platformName -Step 'archive' -Status 'fail' -Seconds 0
            continue
        }
        $archiveBytes = (Get-Item -LiteralPath $archive).Length
        Write-Host ("---- archive {0} ({1:N0} bytes)" -f $archive, $archiveBytes)

        # 3) 上传 + 解包（远端源码目录每次重建；target 目录不动）。
        & scp -o BatchMode=yes -o ConnectTimeout=10 $archive ("{0}:/tmp/{1}" -f $target, $RemoteArchive) 2>&1 | Out-Null
        if ($LASTEXITCODE -ne 0) {
            Write-StepResult -PlatformName $platformName -Step 'upload' -Status 'fail' -Seconds 0
            continue
        }
        & ssh -o BatchMode=yes $target ("rm -rf {0} && mkdir -p {0} && tar -xzf /tmp/{1} -C {0}" -f $RemoteRoot, $RemoteArchive) 2>&1 | Out-Null
        if ($LASTEXITCODE -ne 0) {
            Write-StepResult -PlatformName $platformName -Step 'unpack' -Status 'fail' -Seconds 0
            continue
        }
        # 快照来自 HEAD：如果 scripts/verify.sh 还没进提交，远端就没有入口可跑（先 commit）。
        & ssh -o BatchMode=yes $target ("test -f {0}/scripts/verify.sh" -f $RemoteRoot) 2>&1 | Out-Null
        if ($LASTEXITCODE -ne 0) {
            Write-StepResult -PlatformName $platformName -Step 'unpack' -Status 'fail' -Seconds 0 -Detail ' (HEAD snapshot has no scripts/verify.sh — commit first)'
            if (-not $KeepRemote) {
                & ssh -o BatchMode=yes $target ("rm -rf {0} /tmp/{1}" -f $RemoteRoot, $RemoteArchive) 2>&1 | Out-Null
            }
            Remove-Item -LiteralPath $archive -Force -ErrorAction SilentlyContinue
            continue
        }

        # 4) 远端跑 verify.sh（日志回传本地 .review/remote-<platform>.log）。
        $log = Join-Path $ReviewDir ("remote-{0}.log" -f $name)
        $tsvRemote = "$RemoteRoot/.review/summary.tsv"
        $verifyArgs = "--platform $name --tsv $tsvRemote"
        if ($Bench) {
            $verifyArgs += ' --bench'
        }
        $remoteCommand = "cd $RemoteRoot && CARGO_TARGET_DIR=$RemoteTargetDir bash scripts/verify.sh $verifyArgs"
        Write-Host ("---- remote: {0}" -f $remoteCommand)
        $started = Get-Date
        & ssh -o BatchMode=yes -o ServerAliveInterval=30 $target $remoteCommand 2>&1 | Tee-Object -FilePath $log | Out-Host
        $remoteCode = $LASTEXITCODE
        $remoteSeconds = ((Get-Date) - $started).TotalSeconds
        Write-Host ("---- remote log: {0} (ssh exit {1}, {2:N1}s)" -f $log, $remoteCode, $remoteSeconds)

        # 5) 取回机器可读结果（TSV：platform<TAB>step<TAB>status<TAB>seconds）。
        $tsvLines = & ssh -o BatchMode=yes $target ("cat {0}" -f $tsvRemote) 2>&1
        if ($LASTEXITCODE -ne 0) {
            Write-StepResult -PlatformName $platformName -Step 'verify.sh' -Status 'fail' -Seconds $remoteSeconds -Detail ' (no summary.tsv)'
        }
        else {
            $parsed = 0
            foreach ($line in @($tsvLines)) {
                if ([string]::IsNullOrWhiteSpace($line)) { continue }
                $fields = $line -split "`t"
                if ($fields.Count -lt 4) { continue }
                $parsed++
                Write-StepResult -PlatformName $platformName -Step $fields[1] -Status $fields[2] -Seconds ([double]$fields[3])
            }
            if ($parsed -eq 0) {
                Write-StepResult -PlatformName $platformName -Step 'verify.sh' -Status 'fail' -Seconds $remoteSeconds -Detail ' (empty summary.tsv)'
            }
        }

        # 6) 清理（-KeepRemote 保留源码目录；target 目录始终保留以便增量编译）。
        if (-not $KeepRemote) {
            & ssh -o BatchMode=yes $target ("rm -rf {0} /tmp/{1}" -f $RemoteRoot, $RemoteArchive) 2>&1 | Out-Null
        }
        Remove-Item -LiteralPath $archive -Force -ErrorAction SilentlyContinue
    }
}

# ── 汇总 ────────────────────────────────────────────────────────────────────
Write-Host ''
Write-Host '== verification summary ==' -ForegroundColor Cyan
$script:Results |
    Select-Object Platform, Step, @{ Name = 'Status'; Expression = { Get-Mark $_.Status } }, Seconds |
    Format-Table -AutoSize |
    Out-Host

$failed = @($script:Results | Where-Object { $_.Status -eq 'fail' })
$skipped = @($script:Results | Where-Object { $_.Status -eq 'skip' })
if ($failed.Count -gt 0) {
    Write-Host ("RESULT: FAIL ({0} failed step(s), {1} skipped)" -f $failed.Count, $skipped.Count) -ForegroundColor Red
    exit 1
}
Write-Host ("RESULT: PASS ({0} skipped step(s))" -f $skipped.Count) -ForegroundColor Green
exit 0
