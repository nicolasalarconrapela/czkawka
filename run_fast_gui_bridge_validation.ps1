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

Run-Step "1/3 - GUI Fast/Legacy compatibility bridge" {
    .\kdev1.ps1 test connect_scan::fast_duplicate_gui::tests:: -FastEngine
}

Run-Step "2/3 - Duplicate Engine + exclusion compatibility" {
    .\kdev1.ps1 test duplicate_engine:: -FastEngine
}

Run-Step "3/3 - Full Krokiet suite with Fast Engine" {
    .\kdev1.ps1 test -FastEngine
}

Write-Host ""
Write-Host "============================================================"
Write-Host " FAST GUI BRIDGE VALIDATION: OK"
Write-Host "============================================================"
