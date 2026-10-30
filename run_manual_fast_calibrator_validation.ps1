param()

$ErrorActionPreference = "Stop"

Write-Host ""
Write-Host "============================================================"
Write-Host " KROKIET - MANUAL FAST CALIBRATOR VALIDATION"
Write-Host "============================================================"
Write-Host ""

$required = @(
    ".\krokiet\ui\screens\tool_settings.slint",
    ".\krokiet\ui\globals\callabler.slint",
    ".\krokiet\src\connect_scan.rs"
)

foreach ($file in $required) {
    if (-not (Test-Path $file)) {
        throw "Missing $file"
    }
}

$toolSettings = Get-Content ".\krokiet\ui\screens\tool_settings.slint" -Raw
$callabler = Get-Content ".\krokiet\ui\globals\callabler.slint" -Raw
$connectScan = Get-Content ".\krokiet\src\connect_scan.rs" -Raw

if ($toolSettings -notmatch "Callabler\.calibrate_fast_engine") {
    throw "Tool settings does not invoke calibrate_fast_engine"
}
if ($callabler -notmatch "callback calibrate_fast_engine") {
    throw "Callabler does not expose calibrate_fast_engine"
}
if ($connectScan -notmatch "calibrate_and_store_mode") {
    throw "Rust connector does not call the independent tuning calibrator"
}
if ($connectScan -notmatch "BTreeMap::<String, PathBuf>") {
    throw "Rust connector does not deduplicate included paths by storage"
}

Write-Host "[1/4] Fast Engine build check"
& .\kdev.ps1 check -FastEngine
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ""
Write-Host "[2/4] Tuning tests"
& .\kdev.ps1 test "duplicate_engine::tuning::tests::" -FastEngine
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ""
Write-Host "[3/4] Full Fast Engine tests"
& .\kdev.ps1 test -FastEngine
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ""
Write-Host "[4/4] Legacy build check"
& .\kdev.ps1 check
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ""
Write-Host "MANUAL FAST CALIBRATOR VALIDATION: OK"
Write-Host ""
Write-Host "Manual UI:"
Write-Host "  1. Open Duplicate Files settings"
Write-Host "  2. Add an included path"
Write-Host "  3. Click 'Calibrar Fast Engine'"
Write-Host "  4. Confirm verifier-tuning.json is created/updated"
Write-Host "  5. Run the same Fast scan and confirm the profile is reused"
