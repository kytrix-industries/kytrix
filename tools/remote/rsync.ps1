<#
.SYNOPSIS
    把 `crates/ui_core` 与 workspace 清单同步到远端主机（Linux / macOS），并校验内容一致。

.DESCRIPTION
    Windows 本机无 rsync，使用 tar over ssh 传输。

    **只传 `Cargo.toml` / `Cargo.lock` / `crates/ui_core`（含 examples）**：
    - `ui_core` 不依赖任何其它 workspace crate（依赖全是外部 crate），因此不需要同步兄弟 crate；
    - 整仓传输不可行：`target/` 有 160+ GB / 12 万文件，`crates/widgets` 1.5 GB 是素材。

    **每次都传全量**。旧实现用 `git diff --name-only HEAD` 做增量，但那只报告 *未提交* 的
    改动 —— 一旦提交（每轮工作的常态），增量为空，远端就悄悄停在旧代码上，于是「三平台
    验证」跑的是陈旧源码。这类静默失配远比几秒钟的传输代价严重，因此取消增量。

    同步结束后用 sha256 清单比对两端，不一致立即报错退出。

    -Target linux  -> chenbaiyu@192.168.130.128:~/kytrix
    -Target macos  -> chenbaiyu@192.168.1.6:~/kytrix

.EXAMPLE
    ./tools/remote/rsync.ps1 -Target linux
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('linux', 'macos')]
    [string]$Target
)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
Set-Location $repo

$hostName = if ($Target -eq 'linux') { '192.168.130.128' } else { '192.168.1.6' }
$remote = "chenbaiyu@$hostName"
$remoteDir = 'kytrix'
$sshArgs = @('-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=no')

function Invoke-Remote([string]$Command) {
    $output = & ssh @sshArgs $remote $Command
    if ($LASTEXITCODE -ne 0) { throw "remote command failed ($LASTEXITCODE): $Command" }
    return $output
}

# 远端仓库若不存在就先 clone 一份（只读镜像，源是本机 checkout 的 HEAD）。
Invoke-Remote "test -d ~/$remoteDir/.git || (git clone --no-hardlinks '$repo' ~/$remoteDir)" | Out-Null

# 需要同步的路径（相对仓库根）。
$syncRoots = @('Cargo.toml', 'Cargo.lock', 'crates/ui_core')

Write-Host "==> sync $($syncRoots -join ', ') to $Target"
# 先删远端 `crates/ui_core` 再整树展开：tar 只「增加/覆盖」，不会删除本地已删掉的文件，
# 残留文件会让后续的 sha256 校验失败（也会让远端编出本地不存在的模块）。
Invoke-Remote "rm -rf ~/$remoteDir/crates/ui_core" | Out-Null
$tarArgs = @('-c', '-f', '-') + $syncRoots
& tar @tarArgs | & ssh @sshArgs $remote "tar -x -f - -C ~/$remoteDir"
if ($LASTEXITCODE -ne 0) { throw "sync failed ($LASTEXITCODE)" }

# ── 校验：两端对同一批文件算 sha256，排序后比对 ─────────────────────────────────
$candidateFiles = @(Get-ChildItem -Path (Join-Path $repo 'crates\ui_core') -Recurse -File -ErrorAction SilentlyContinue)
$candidateFiles += @(Get-ChildItem -Path $repo -File -Filter 'Cargo.*' -ErrorAction SilentlyContinue)

$localLines = foreach ($file in $candidateFiles) {
    $rel = $file.FullName.Substring($repo.Length + 1).Replace('\', '/')
    $hash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $rel"
}
$localManifest = ($localLines | Sort-Object -Unique) -join "`n"
$localDigest = [System.BitConverter]::ToString(
    [System.Security.Cryptography.SHA256]::Create().ComputeHash(
        [System.Text.Encoding]::UTF8.GetBytes($localManifest)
    )
).Replace('-', '').ToLowerInvariant()

# 根清单用 `printf` 明确给出无 `./` 前缀的路径，否则 `find .` 会输出 `./Cargo.toml`，
# 与本地清单的行格式不一致，校验会假报失败。
$remoteManifest = Invoke-Remote "cd ~/$remoteDir && { find crates/ui_core -type f -print0; printf 'Cargo.toml\0Cargo.lock\0'; } | sort -z | xargs -0 sha256sum; true"
$remoteDigest = [System.BitConverter]::ToString(
    [System.Security.Cryptography.SHA256]::Create().ComputeHash(
        [System.Text.Encoding]::UTF8.GetBytes((($remoteManifest | Sort-Object -Unique) -join "`n"))
    )
).Replace('-', '').ToLowerInvariant()

if ($localDigest -ne $remoteDigest) {
    throw "sync verification failed: local $localDigest != remote $remoteDigest（远端源码与本地不一致）"
}

$fileCount = ($localManifest -split "`n").Count
Write-Host "==> synced $fileCount file(s) to $Target (verified $localDigest)"
