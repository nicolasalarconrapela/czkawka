[CmdletBinding()]
param(
    [ValidateSet("All", "ProfileFlow", "Calibration", "ProfileReuse")]
    [string]$Mode = "All",
    [string]$Path = "Q:\KrokietBenchmark",
    [string]$TuningStore = ".\temp-readonly-tuning.json",
    [int]$ExpectedWorkers = 0,
    [switch]$SkipRealProfileReuse
)

$ErrorActionPreference = "Stop"
$script:ExecutedStages = 0
$script:TotalStages = 0

function Write-Section {
    param([string]$Title)
    Write-Host ""
    Write-Host "============================================================"
    Write-Host " $Title"
    Write-Host "============================================================"
}

function Invoke-KrokietTest {
    param([string]$Title, [string]$Filter)
    $script:ExecutedStages++
    Write-Section "$($script:ExecutedStages)/$($script:TotalStages) - $Title"
    & .\kdev.ps1 test $Filter -FastEngine
    $code = $LASTEXITCODE
    if ($code -ne 0) {
        Write-Host ""
        Write-Host "[ERROR] $Title (exit=$code)" -ForegroundColor Red
        exit $code
    }
    Write-Host ""
    Write-Host "[OK] $Title" -ForegroundColor Green
}

function Invoke-ProfileReuseRealCheck {
    $script:ExecutedStages++
    Write-Section "$($script:ExecutedStages)/$($script:TotalStages) - Validacion real del perfil persistido"

    & cargo build -p krokiet `
        --bin krokiet-profile-reuse-check `
        --release `
        --no-default-features `
        --features "fast_duplicates,winit_femtovg"

    $code = $LASTEXITCODE
    if ($code -ne 0) {
        Write-Host "[ERROR] No se pudo compilar krokiet-profile-reuse-check (exit=$code)" -ForegroundColor Red
        exit $code
    }

    $checkerArgs = @("--tuning-store", $TuningStore)
    if ($ExpectedWorkers -gt 0) {
        $checkerArgs += @("--expect-workers", "$ExpectedWorkers")
    }
    $checkerArgs += $Path

    & .\target\release\krokiet-profile-reuse-check.exe @checkerArgs
    $code = $LASTEXITCODE
    if ($code -ne 0) {
        Write-Host "[ERROR] La reutilizacion real del perfil fallo (exit=$code)" -ForegroundColor Red
        exit $code
    }

    Write-Host ""
    Write-Host "[OK] Perfil real reutilizado sin recalibrar" -ForegroundColor Green
}

$profileFlowTests = @(
    @{ Title = "E2E: calibracion externa -> reentrada -> scan real"; Filter = "e2e_profile_reentry_persisted_profile_then_real_scan" },
    @{ Title = "E2E: tres reentradas -> EXIT controlado"; Filter = "e2e_profile_reentry_limit_exits_without_running_scan" }
)

$calibrationTests = @(
    @{ Title = "Readonly: no modifica archivos existentes"; Filter = "readonly_dataset_uses_existing_files_without_modifying_them" },
    @{ Title = "Readonly: seleccion y fingerprint deterministas"; Filter = "readonly_dataset_tie_break_and_fingerprint_are_deterministic" },
    @{ Title = "Readonly: rechaza dataset insuficiente"; Filter = "readonly_dataset_rejects_insufficient_existing_files" },
    @{ Title = "Diagnostico invalido: no pisa perfil bueno"; Filter = "diagnostic_invalid_calibration_returns_report_without_overwriting_profile" },
    @{ Title = "Persistencia: conserva perfil anterior cuando corresponde"; Filter = "diagnostic_persistence_reports_retained_existing_profile" },
    @{ Title = "Pairwise: informa override solo si cambia seleccion"; Filter = "pairwise_override_is_reported_only_when_final_selection_changes" },
    @{ Title = "Pairwise: bloquea promocion fuera del margen agregado"; Filter = "pairwise_promotion_outside_aggregate_tie_margin_is_blocked" },
    @{ Title = "Pairwise: contradiccion fuerza rescate"; Filter = "blocked_pairwise_promotion_invalidates_pass_for_rescue" },
    @{ Title = "Confianza: ruido global limita alta a media"; Filter = "high_global_noise_caps_high_confidence_at_medium" },
    @{ Title = "Persistencia: perfiles v6 se ignoran tras cambio de politica"; Filter = "format_v6_profiles_are_ignored_after_confidence_policy_change" },
    @{ Title = "Readonly E2E: verifier + persistencia sin escrituras en target"; Filter = "readonly_calibration_runs_verifier_and_persists_profile_without_target_writes" }
)

$profileReuseTests = @(
    @{ Title = "Scan normal reutiliza perfil sin calibrar"; Filter = "scan_resolution_reuses_profile_without_any_calibration_probe" }
)

$runProfileFlow = $Mode -in @("All", "ProfileFlow")
$runCalibration = $Mode -in @("All", "Calibration")
$runProfileReuse = $Mode -in @("All", "ProfileReuse")

$selectedTests = @()
if ($runProfileFlow) { $selectedTests += $profileFlowTests }
if ($runCalibration) { $selectedTests += $calibrationTests }
if ($runProfileReuse) { $selectedTests += $profileReuseTests }

# The old scripts ran service/tuning/verify suites independently and then
# duplicate_engine again. Run the complete engine suite only once in All mode.
$runFullEngineSuite = $Mode -eq "All"
$runRealReuse = $runProfileReuse -and -not $SkipRealProfileReuse

$script:TotalStages = $selectedTests.Count
if ($runFullEngineSuite) { $script:TotalStages++ }
if ($runRealReuse) { $script:TotalStages++ }

Write-Host "==========================================================="
Write-Host " KROKIET - FAST DUPLICATES VALIDATION"
Write-Host "==========================================================="
Write-Host " Mode            : $Mode"
Write-Host " Test stages     : $($selectedTests.Count)"
Write-Host " Full engine     : $runFullEngineSuite"
Write-Host " Real reuse      : $runRealReuse"
if ($runRealReuse) {
    Write-Host " Path            : $Path"
    Write-Host " Tuning store    : $TuningStore"
    if ($ExpectedWorkers -gt 0) { Write-Host " Expected workers: $ExpectedWorkers" }
}
Write-Host ""

foreach ($test in $selectedTests) {
    Invoke-KrokietTest -Title $test.Title -Filter $test.Filter
}

if ($runFullEngineSuite) {
    Invoke-KrokietTest -Title "Suite completa duplicate_engine" -Filter "duplicate_engine::"
}

if ($runRealReuse) {
    Invoke-ProfileReuseRealCheck
}

Write-Host ""
Write-Host "===========================================================" -ForegroundColor Green
Write-Host " KROKIET FAST DUPLICATES - TODO [OK]" -ForegroundColor Green
Write-Host " Etapas ejecutadas: $script:ExecutedStages/$script:TotalStages" -ForegroundColor Green
Write-Host "===========================================================" -ForegroundColor Green
