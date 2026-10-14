param(
    [string]$Path = "Q:\KrokietBenchmark",
    [string]$TuningStore = ".\temp-readonly-tuning.json",
    [int]$ExpectedWorkers = 0
)

$ErrorActionPreference = "Stop"

function Invoke-TestStage {
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
Write-Host " KROKIET 3.4.5 - PROFILE REUSE VALIDATION"
Write-Host "==========================================================="

Invoke-TestStage 1 3 `
    "Scan normal reutiliza perfil sin calibrar" `
    "scan_resolution_reuses_profile_without_any_calibration_probe"

Invoke-TestStage 2 3 `
    "E2E: perfil persistido llega al scan real + ExactVerifier" `
    "e2e_profile_reentry_persisted_profile_then_real_scan"

Write-Host ""
Write-Host "============================================================"
Write-Host " 3/3 - Validacion real del perfil persistido"
Write-Host "============================================================"

cargo build -p krokiet `
    --bin krokiet-profile-reuse-check `
    --release `
    --no-default-features `
    --features "fast_duplicates,winit_femtovg"

if ($LASTEXITCODE -ne 0) {
    Write-Host "[ERROR] No se pudo compilar krokiet-profile-reuse-check" -ForegroundColor Red
    exit $LASTEXITCODE
}

$checkerArgs = @(
    "--tuning-store", $TuningStore
)
if ($ExpectedWorkers -gt 0) {
    $checkerArgs += @("--expect-workers", "$ExpectedWorkers")
}
$checkerArgs += $Path

& .\target\release\krokiet-profile-reuse-check.exe @checkerArgs
$code = $LASTEXITCODE
if ($code -ne 0) {
    Write-Host "[ERROR] 3/3 - La reutilizacion real del perfil fallo (exit=$code)" -ForegroundColor Red
    exit $code
}

Write-Host ""
Write-Host "[OK] 3/3 - El scan resolver reutiliza el perfil sin recalibrar" -ForegroundColor Green
Write-Host ""
Write-Host "===========================================================" -ForegroundColor Green
Write-Host " KROKIET 3.4.5 PROFILE REUSE - TODO [OK]" -ForegroundColor Green
Write-Host "===========================================================" -ForegroundColor Green
