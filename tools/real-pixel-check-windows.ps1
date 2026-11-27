$ErrorActionPreference = 'Stop'

$Root = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
if ($env:CARGO_TARGET_DIR) {
    $Target = $env:CARGO_TARGET_DIR
} else {
    $Target = Join-Path $Root 'target'
}
$env:CARGO_TARGET_DIR = $Target
$OsRoot = Join-Path $Target 'real-pixels/windows'
$SourceSha = ''
$SourceIdPath = Join-Path $Root 'tools/source-id.txt'
if (Test-Path $SourceIdPath) {
    $SourceLine = Get-Content $SourceIdPath | Where-Object { $_ -like 'diff_sha=*' } | Select-Object -First 1
    if ($SourceLine) {
        $SourceSha = ($SourceLine -replace '^diff_sha=', '').Trim()
        if ($SourceSha.Length -gt 8) { $SourceSha = $SourceSha.Substring(0, 8) }
    }
}
if (-not $SourceSha) { $SourceSha = 'unknown' }
$RunStamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
$OsDir = Join-Path $OsRoot ("{0}-{1}" -f $RunStamp, $SourceSha)
New-Item -ItemType Directory -Force -Path $OsDir | Out-Null
Set-Content -Path (Join-Path $OsRoot 'latest.txt') -Value $OsDir
$env:KYTRIX_REAL_PIXEL_EVIDENCE = $OsDir
$Log = Join-Path $OsDir 'run.log'
"== kytrix real-pixel gate $(Get-Date -Format o) ==" | Out-File -FilePath $Log -Encoding utf8
"root=$Root" | Out-File -FilePath $Log -Append -Encoding utf8
"host=$([System.Environment]::OSVersion.VersionString)" | Out-File -FilePath $Log -Append -Encoding utf8
"CARGO_TARGET_DIR=$Target" | Out-File -FilePath $Log -Append -Encoding utf8
"precondition: Windows capture uses PowerShell System.Drawing CopyFromScreen of the virtual screen" | Out-File -FilePath $Log -Append -Encoding utf8
"note: under a busy/concurrent desktop (screenshots, parallel gates, IDE load) --debug-stack can exceed the 300 s gate timeout; the script retries it up to 3 attempts and a single timeout is retried without changing the gate result" | Out-File -FilePath $Log -Append -Encoding utf8
$SourceIdFile = Join-Path $Root 'tools/source-id.txt'
if (Test-Path $SourceIdFile) {
    $SourceId = 'source_id=' + ((Get-Content $SourceIdFile) -join ' ')
    $SourceId | Out-File -FilePath $Log -Append -Encoding utf8
}

function Write-Log([string]$Line) {
    $Line | Out-File -FilePath $Log -Append -Encoding utf8
}

function Invoke-Gate([string]$Name, [string[]]$GateArgs) {
    $GateLog = Join-Path $OsDir ("{0}.log" -f $Name)
    $ErrorLog = Join-Path $OsDir ("{0}.stderr.log" -f $Name)
    Write-Log "== gate ${Name}: gallery $($GateArgs -join ' ')"
    $Process = Start-Process -FilePath $Exe -ArgumentList $GateArgs -PassThru -RedirectStandardOutput $GateLog -RedirectStandardError $ErrorLog -NoNewWindow
    if (-not $Process.WaitForExit(300000)) {
        $Process.Kill()
        $Process.WaitForExit()
        Write-Log "gate $Name timeout(300s)"
        return 124
    }
    return $Process.ExitCode
}

Push-Location $Root
try {
    & cargo build -p ui_kit --example gallery *>> $Log
    if ($LASTEXITCODE -ne 0) {
        Write-Log 'build FAILED'
        exit 2
    }
    $Exe = Join-Path $Target 'debug/examples/gallery.exe'
    $Fail = 0
    $Gates = @(
        @{ Name = 'pixel-check'; Args = @('--pixel-check') },
        @{ Name = 'wheel-check'; Args = @('--wheel-check') },
        @{ Name = 'real-pixel-check'; Args = @('--real-pixel-check') },
        @{ Name = 'idle-check'; Args = @('--idle-check') },
        @{ Name = 'theme-check'; Args = @('--theme-check') }
    )
    foreach ($Gate in $Gates) {
        $Code = Invoke-Gate $Gate.Name @($Gate.Args)
        Write-Log "gate $($Gate.Name) exit=$Code"
        Get-Content (Join-Path $OsDir ("{0}.log" -f $Gate.Name)) -Tail 4 | Out-File -FilePath $Log -Append -Encoding utf8
        if ($Code -ne 0) { $Fail = 1 }
    }
    $DebugOk = 0
    for ($Attempt = 1; $Attempt -le 3; $Attempt++) {
        Write-Log "== gate debug-stack attempt $Attempt"
        $Code = Invoke-Gate "debug-stack-$Attempt" @('--debug-stack')
        Write-Log "debug-stack attempt $Attempt exit=$Code"
        Get-Content (Join-Path $OsDir ("debug-stack-{0}.log" -f $Attempt)) -Tail 4 | Out-File -FilePath $Log -Append -Encoding utf8
        if ($Code -eq 0) {
            Copy-Item (Join-Path $OsDir ("debug-stack-{0}.log" -f $Attempt)) (Join-Path $OsDir 'debug-stack.log') -Force
            $DebugOk = 1
            break
        }
    }
    if ($DebugOk -ne 1) { $Fail = 1 }
    Write-Log "gates_done fail=$Fail"
    if ($Fail -ne 0) { exit 3 }
    exit 0
} finally {
    Pop-Location
}
