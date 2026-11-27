$ErrorActionPreference = 'Stop'

$Root = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
Push-Location $Root
try {
    $Revision = (& git rev-parse --short HEAD).Trim()
    $StatusLines = @(& git status --porcelain | Where-Object { $_ -notmatch 'tools/source-id\.txt' } | Where-Object { $_.Trim().Length -gt 0 })
    if ($StatusLines.Count -eq 0) {
        $Status = 'clean'
    } else {
        $Status = "dirty($($StatusLines.Count) files)"
    }
    $DiffSha = (& cmd /d /c "cd /d `"$Root`" && git diff HEAD | git hash-object --stdin").Trim()
    $Content = "revision=$Revision`nstatus=$Status`ndiff_sha=$DiffSha`n"
    [System.IO.File]::WriteAllText((Join-Path $Root 'tools/source-id.txt'), $Content, [System.Text.UTF8Encoding]::new($false))
    Write-Output "revision=$Revision"
    Write-Output "status=$Status"
    Write-Output "diff_sha=$DiffSha"
} finally {
    Pop-Location
}
