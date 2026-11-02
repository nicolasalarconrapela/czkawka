param()

$ErrorActionPreference = "Stop"

Write-Host ""
Write-Host "============================================================"
Write-Host " KROKIET - FAST CALIBRATION VISUAL PROGRESS VALIDATION"
Write-Host "============================================================"
Write-Host ""

$source = ".\krokiet\src\connect_scan.rs"
if (-not (Test-Path $source)) {
    throw "Missing $source"
}

$content = Get-Content $source -Raw

$requiredPatterns = @(
    "publish_fast_calibration_progress",
    "volumen \{current_number\}/\{total\}",
    "all_progress: overall_percent",
    "app\.set_processing\(true\)"
)

foreach ($pattern in $requiredPatterns) {
    if ($content -notmatch $pattern) {
        throw "Missing visual progress implementation pattern: $pattern"
    }
}

Write-Host "[1/3] Fast Engine build check"
& .\kdev.ps1 check -FastEngine
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ""
Write-Host "[2/3] Fast Engine tests"
& .\kdev.ps1 test -FastEngine
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ""
Write-Host "[3/3] Legacy build check"
& .\kdev.ps1 check
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ""
Write-Host "FAST CALIBRATION VISUAL PROGRESS VALIDATION: OK"
Write-Host ""
Write-Host "Manual UI:"
Write-Host "  1. Run: .\kdev.ps1 run -FastEngine"
Write-Host "  2. Add one or more included paths"
Write-Host "  3. Click 'Calibrar Fast Engine'"
Write-Host "  4. Confirm the normal Krokiet progress area appears"
Write-Host "  5. Confirm text shows 'volumen N/T'"
Write-Host "  6. With multiple volumes, confirm overall progress advances after each volume"
