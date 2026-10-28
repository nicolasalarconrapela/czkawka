$ErrorActionPreference = "Stop"

function Run-Step {
    param(
        [string]$Name,
        [scriptblock]$Command
    )

    Write-Host ""
    Write-Host "============================================================"
    Write-Host " $Name"
    Write-Host "============================================================"

    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "$Name failed with exit code $LASTEXITCODE"
    }
}

Run-Step "1/4 - Fast GUI routing/state tests" {
    .\kdev1.ps1 test connect_scan::fast_duplicate_gui::tests:: -FastEngine
}

Run-Step "2/4 - Duplicate Engine regression suite" {
    .\kdev1.ps1 test duplicate_engine:: -FastEngine
}

Run-Step "3/4 - Full Krokiet suite with Fast Engine" {
    .\kdev1.ps1 test -FastEngine
}

Run-Step "4/4 - Full Krokiet legacy build/tests without Fast Engine" {
    .\kdev1.ps1 test
}

Write-Host ""
Write-Host "============================================================"
Write-Host " FAST GUI ACTIVATION VALIDATION: OK"
Write-Host "============================================================"
