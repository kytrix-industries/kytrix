<#
.SYNOPSIS
    在远端主机上执行 cargo 命令并把输出回显到本机。

.DESCRIPTION
    封装 `ssh <host> "cd ~/kytrix && <cargo ...>"`，并统一设置环境变量：
    - CARGO_TERM_COLOR=never，避免 ANSI 污染本机输出
    - 可选 `-Sync` 先同步源码

.PARAMETER Target
    linux / macos

.PARAMETER CargoArgs
    传给 cargo 的参数，例如 "check -p ui_core --all-targets"

.EXAMPLE
    ./tools/remote/cargo.ps1 -Target linux -CargoArgs "check -p ui_core --all-targets"
    ./tools/remote/cargo.ps1 -Target macos -Sync -CargoArgs "test -p ui_core --features test-support"
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('linux', 'macos')]
    [string]$Target,

    [Parameter(Mandatory = $true, ValueFromRemainingArguments = $false)]
    [string]$CargoArgs,

    [switch]$Sync,

    # 远端 cargo 二进制（macOS 在 ~/.cargo/bin，Linux 同样）
    [string]$Cargo = 'cargo'
)

$ErrorActionPreference = 'Stop'

if ($Sync) { & (Join-Path $PSScriptRoot 'rsync.ps1') -Target $Target }

$hostName = if ($Target -eq 'linux') { '192.168.130.128' } else { '192.168.1.6' }
$remote = "chenbaiyu@$hostName"

# 非交互式 ssh 不会加载 ~/.cargo/env，显式把 ~/.cargo/bin 前置到 PATH。
$inner = "export PATH=`"`$HOME/.cargo/bin:`$PATH`" && cd ~/kytrix && CARGO_TERM_COLOR=never $Cargo $CargoArgs"
Write-Host "==> [$Target] cargo $CargoArgs" -ForegroundColor Cyan
& ssh -o BatchMode=yes -o StrictHostKeyChecking=no $remote $inner
exit $LASTEXITCODE
