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
Write-Host " KROKIET 3.4.4 - GLOBAL NOISE CONFIDENCE GUARD"
Write-Host "==========================================================="

Invoke-Stage 1 13 `
    "Readonly dataset: no modifica archivos existentes" `
    "readonly_dataset_uses_existing_files_without_modifying_them"

Invoke-Stage 2 13 `
    "Readonly dataset: seleccion y fingerprint deterministas" `
    "readonly_dataset_tie_break_and_fingerprint_are_deterministic"

Invoke-Stage 3 13 `
    "Readonly dataset: rechaza dataset insuficiente" `
    "readonly_dataset_rejects_insufficient_existing_files"

Invoke-Stage 4 13 `
    "Diagnostico invalido: devuelve informe y no pisa perfil bueno" `
    "diagnostic_invalid_calibration_returns_report_without_overwriting_profile"

Invoke-Stage 5 13 `
    "Persistencia: informa cuando conserva el perfil anterior" `
    "diagnostic_persistence_reports_retained_existing_profile"

Invoke-Stage 6 13 `
    "Decision: detecta override de la confirmacion pareada" `
    "pairwise_override_is_reported_only_when_final_selection_changes"

Invoke-Stage 7 13 `
    "Decision: bloquea promocion fuera del margen agregado" `
    "pairwise_promotion_outside_aggregate_tie_margin_is_blocked"

Invoke-Stage 8 13 `
    "Decision: contradiccion pareada fuerza rescate" `
    "blocked_pairwise_promotion_invalidates_pass_for_rescue"

Invoke-Stage 9 13 `
    "Confianza: ruido global limita alta a media" `
    "high_global_noise_caps_high_confidence_at_medium"

Invoke-Stage 10 13 `
    "Persistencia: perfiles v6 se recalibran con la nueva politica" `
    "format_v6_profiles_are_ignored_after_confidence_policy_change"

Invoke-Stage 11 13 `
    "Readonly E2E: verifier + persistencia sin escrituras en target" `
    "readonly_calibration_runs_verifier_and_persists_profile_without_target_writes"

Invoke-Stage 12 13 `
    "Suite completa tuning" `
    "duplicate_engine::tuning::"

Invoke-Stage 13 13 `
    "Suite completa duplicate_engine" `
    "duplicate_engine::"

Write-Host ""
Write-Host "===========================================================" -ForegroundColor Green
Write-Host " KROKIET 3.4.4 GLOBAL NOISE GUARD - TODO [OK]" -ForegroundColor Green
Write-Host "===========================================================" -ForegroundColor Green
