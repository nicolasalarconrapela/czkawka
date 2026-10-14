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
Write-Host " KROKIET 3.4.2 - PERSISTENCE + DECISION DIAGNOSTICS"
Write-Host "==========================================================="

Invoke-Stage 1 9 `
    "Readonly dataset: no modifica archivos existentes" `
    "readonly_dataset_uses_existing_files_without_modifying_them"

Invoke-Stage 2 9 `
    "Readonly dataset: seleccion y fingerprint deterministas" `
    "readonly_dataset_tie_break_and_fingerprint_are_deterministic"

Invoke-Stage 3 9 `
    "Readonly dataset: rechaza dataset insuficiente" `
    "readonly_dataset_rejects_insufficient_existing_files"

Invoke-Stage 4 9 `
    "Diagnostico invalido: devuelve informe y no pisa perfil bueno" `
    "diagnostic_invalid_calibration_returns_report_without_overwriting_profile"

Invoke-Stage 5 9 `
    "Persistencia: informa cuando conserva el perfil anterior" `
    "diagnostic_persistence_reports_retained_existing_profile"

Invoke-Stage 6 9 `
    "Decision: detecta override de la confirmacion pareada" `
    "pairwise_override_is_reported_only_when_final_selection_changes"

Invoke-Stage 7 9 `
    "Readonly E2E: verifier + persistencia sin escrituras en target" `
    "readonly_calibration_runs_verifier_and_persists_profile_without_target_writes"

Invoke-Stage 8 9 `
    "Suite completa tuning" `
    "duplicate_engine::tuning::"

Invoke-Stage 9 9 `
    "Suite completa duplicate_engine" `
    "duplicate_engine::"

Write-Host ""
Write-Host "===========================================================" -ForegroundColor Green
Write-Host " KROKIET 3.4.2 DIAGNOSTICS - TODO [OK]" -ForegroundColor Green
Write-Host "===========================================================" -ForegroundColor Green
