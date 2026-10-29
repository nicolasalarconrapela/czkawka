param()

$ErrorActionPreference = "Stop"

Write-Host ""
Write-Host "============================================================"
Write-Host " KROKIET - FAST ENGINE INCOMPATIBILITY MODAL VALIDATION"
Write-Host "============================================================"
Write-Host ""

if (-not (Test-Path ".\kdev.ps1")) {
    throw "Run this script from the repository root."
}

$duplicateFile = ".\krokiet\src\connect_scan\duplicate.rs"
if (-not (Test-Path $duplicateFile)) {
    throw "Missing $duplicateFile"
}

$content = Get-Content $duplicateFile -Raw
if ($content -notmatch "finish_fast_incompatible_scan") {
    throw "The incompatibility guard is not present in duplicate.rs"
}
if ($content -notmatch "invoke_show_error_popup") {
    throw "The incompatibility guard does not invoke the Krokiet error popup"
}

Write-Host "[1/3] Fast Engine build check"
& .\kdev.ps1 check -FastEngine
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ""
Write-Host "[2/3] Fast GUI planner tests"
& .\kdev.ps1 test "connect_scan::fast_duplicate_gui::tests::" -FastEngine
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ""
Write-Host "[3/3] Legacy build check"
& .\kdev.ps1 check
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ""
Write-Host "FAST ENGINE INCOMPATIBILITY MODAL VALIDATION: OK"
Write-Host ""
Write-Host "Manual UI check:"
Write-Host "  Motor  : Fast Engine"
Write-Host "  Method : Size"
Write-Host "  Action : Scan"
Write-Host ""
Write-Host "Expected:"
Write-Host "  - no fclones scan"
Write-Host "  - no Czkawka fallback"
Write-Host "  - visible error modal explaining Hash + Blake3 requirement"
