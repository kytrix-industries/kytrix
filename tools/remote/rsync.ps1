<#
.SYNOPSIS
    把当前工作区的 ui_core 相关源码同步到远端主机（Linux / macOS）。

.DESCRIPTION
    Windows 本机无 rsync，使用 tar over ssh 做增量传输：只传 `git diff` 报告变更的文件
    加上未跟踪的新文件，避免整仓 5 万行传输。

    -Target linux  -> chenbaiyu@192.168.130.128:~/kytrix
    -Target macos  -> chenbaiyu@192.168.1.6:~/kytrix

.PARAMETER All
    同步整个仓库（首次或大改动时用），而不是只同步变更文件。

.EXAMPLE
    ./tools/remote/rsync.ps1 -Target linux
    ./tools/remote/rsync.ps1 -Target macos -All
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('linux', 'macos')]
    [string]$Target,

    [switch]$All
)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
Set-Location $repo

$hostName = if ($Target -eq 'linux') { '192.168.130.128' } else { '192.168.1.6' }
$remote = "chenbaiyu@$hostName"
$remoteDir = 'kytrix'

function Invoke-Remote([string]$Command) {
    & ssh -o BatchMode=yes -o StrictHostKeyChecking=no $remote $Command
    if ($LASTEXITCODE -ne 0) { throw "remote command failed ($LASTEXITCODE): $Command" }
}

# 远端仓库若不存在就先 clone 一份（只读镜像，源是本机 checkout 的 HEAD）。
Invoke-Remote "test -d ~/$remoteDir/.git || (git clone --no-hardlinks '$repo' ~/$remoteDir)"

if ($All) {
    Write-Host "==> full sync ($Target)"
    # 排除构建产物与本地审查资料，其余整树传输。
    $tarArgs = @(
        '-c', '-f', '-',
        '--exclude=./target', '--exclude=./.git', '--exclude=./review',
        '--exclude=./node_modules', '--exclude=./crates/ui_core/examples'
        '.'
    )
    & tar @tarArgs | & ssh -o BatchMode=yes -o StrictHostKeyChecking=no $remote "tar -x -f - -C ~/$remoteDir"
} else {
    # 只传 git 报告的变更 + 未跟踪文件。
    $files = @()
    $files += & git diff --name-only HEAD
    $files += & git ls-files --others --exclude-standard
    $files = $files | Where-Object {
        $_ -and (Test-Path $_) -and ($_ -notlike 'review/*') -and ($_ -notlike 'target/*')
    } | Sort-Object -Unique

    if ($files.Count -eq 0) { Write-Host '==> nothing to sync'; return }

    Write-Host "==> syncing $($files.Count) file(s) to $Target"
    $listFile = Join-Path ([System.IO.Path]::GetTempPath()) "uicore-sync-$Target.txt"
    $files | Set-Content -LiteralPath $listFile -Encoding utf8

    & tar -c -f - -T $listFile | & ssh -o BatchMode=yes -o StrictHostKeyChecking=no $remote "tar -x -f - -C ~/$remoteDir"
    Remove-Item -LiteralPath $listFile -Force
}

if ($LASTEXITCODE -ne 0) { throw "sync failed ($LASTEXITCODE)" }
Write-Host "==> synced to $remoteDir on $Target"
