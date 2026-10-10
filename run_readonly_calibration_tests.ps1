$ErrorActionPreference = "Stop"

function Invoke-Stage {
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
Write-Host " KROKIET 3.4.0 - READONLY CALIBRATION VALIDATION"
Write-Host "==========================================================="

Invoke-Stage 1 5 `
    "Readonly dataset: no modifica archivos existentes" `
    "readonly_dataset_uses_existing_files_without_modifying_them"

Invoke-Stage 2 5 `
    "Readonly dataset: rechaza dataset insuficiente" `
    "readonly_dataset_rejects_insufficient_existing_files"

Invoke-Stage 3 5 `
    "Readonly E2E: verifier + persistencia sin escrituras en target" `
    "readonly_calibration_runs_verifier_and_persists_profile_without_target_writes"

Invoke-Stage 4 5 `
    "Suite completa tuning" `
    "duplicate_engine::tuning::"

Invoke-Stage 5 5 `
    "Suite completa duplicate_engine" `
    "duplicate_engine::"

Write-Host ""
Write-Host "===========================================================" -ForegroundColor Green
Write-Host " KROKIET READONLY CALIBRATION - TODO [OK]" -ForegroundColor Green
Write-Host "===========================================================" -ForegroundColor Green
