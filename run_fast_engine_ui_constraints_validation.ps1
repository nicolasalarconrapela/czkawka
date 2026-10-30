param()
$ErrorActionPreference = "Stop"
Write-Host ""
Write-Host "============================================================"
Write-Host " KROKIET - FAST ENGINE UI CONSTRAINTS VALIDATION"
Write-Host "============================================================"
Write-Host ""

$content = Get-Content ".\krokiet\src\connect_scan\duplicate.rs" -Raw
$gui = Get-Content ".\krokiet\src\connect_scan\fast_duplicate_gui.rs" -Raw
$ui = Get-Content ".\krokiet\ui\screens\tool_settings.slint" -Raw

if ($content -notmatch "finish_fast_incompatible_scan") { throw "Safety modal guard missing" }
if ($content -notmatch "effective_duplicate_matching") { throw "Effective Fast matching guard missing" }
if ($content -notmatch "invoke_show_error_popup") { throw "Error popup hook missing" }
if ($gui -notmatch "forced_fast_engine_uses_hash_and_blake3") { throw "Fast matching tests missing" }
if ($ui -notmatch "duplicates_sub_engine_index != 2") { throw "Fast UI constraint missing" }

Write-Host "[1/4] Fast build check"
& .\kdev.ps1 check -FastEngine
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "[2/4] Fast GUI tests"
& .\kdev.ps1 test "connect_scan::fast_duplicate_gui::tests::" -FastEngine
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "[3/4] Full Fast tests"
& .\kdev.ps1 test -FastEngine
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "[4/4] Legacy build check"
& .\kdev.ps1 check
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ""
Write-Host "FAST ENGINE UI CONSTRAINTS VALIDATION: OK"
Write-Host ""
Write-Host "Manual UI check:"
Write-Host "  1. Czkawka -> Method Size"
Write-Host "  2. Switch to Fast Engine"
Write-Host "  3. Method/hash selectors must disappear"
Write-Host "  4. UI must show Hash + Blake3"
Write-Host "  5. Scan must use Fast Engine"
Write-Host "  6. Switch back to Czkawka"
Write-Host "  7. Previous Size selection should reappear"
