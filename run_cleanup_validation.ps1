param(
    [switch]$Full
)

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

Run-Step "1/3 - Service profile/tuning flow" {
    .\kdev1.ps1 test duplicate_engine::service::tests:: -FastEngine
}

Run-Step "2/3 - Duplicate engine suite" {
    .\kdev1.ps1 test duplicate_engine:: -FastEngine
}

if ($Full) {
    Run-Step "3/3 - Full Krokiet tests" {
        .\kdev1.ps1 test -FastEngine
    }
} else {
    Write-Host ""
    Write-Host "[SKIP] Full Krokiet suite. Use -Full to run it."
}

Write-Host ""
Write-Host "============================================================"
Write-Host " CLEANUP VALIDATION: OK"
Write-Host "============================================================"
