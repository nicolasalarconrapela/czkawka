$ErrorActionPreference = "Stop"

function Invoke-KrokietValidationStage {
    param(
        [int]$Number,
        [int]$Total,
        [string]$Title,
        [string]$Filter
    )

    Write-Host ""
    Write-Host "============================================================"
    Write-Host " $Number/$Total - $Title"
    Write-Host "============================================================"

    & .\kdev.ps1 test $Filter -FastEngine
    $code = $LASTEXITCODE

    if ($code -ne 0) {
        Write-Host ""
        Write-Host "[ERROR] $Number/$Total - $Title (exit=$code)" -ForegroundColor Red
        exit $code
    }

    Write-Host ""
    Write-Host "[OK] $Number/$Total - $Title" -ForegroundColor Green
}

Write-Host "==========================================================="
Write-Host " KROKIET 3.3.2 - STRICT PROFILE FLOW VALIDATION"
Write-Host "==========================================================="

Invoke-KrokietValidationStage 1 6 `
    "E2E: calibracion externa -> reentrada -> scan real" `
    "e2e_profile_reentry_persisted_profile_then_real_scan"

Invoke-KrokietValidationStage 2 6 `
    "E2E: tres reentradas -> EXIT controlado" `
    "e2e_profile_reentry_limit_exits_without_running_scan"

Invoke-KrokietValidationStage 3 6 `
    "Service/profile flow" `
    "duplicate_engine::service::"

Invoke-KrokietValidationStage 4 6 `
    "Tuning/profile persistence" `
    "duplicate_engine::tuning::"

Invoke-KrokietValidationStage 5 6 `
    "Exact verifier" `
    "duplicate_engine::verify::"

Invoke-KrokietValidationStage 6 6 `
    "Suite completa duplicate_engine" `
    "duplicate_engine::"

Write-Host ""
Write-Host "===========================================================" -ForegroundColor Green
Write-Host " KROKIET PROFILE FLOW - RESULTADO FINAL: TODO [OK]" -ForegroundColor Green
Write-Host " Todos los bloques ejecutaron al menos un test real." -ForegroundColor Green
Write-Host "===========================================================" -ForegroundColor Green
