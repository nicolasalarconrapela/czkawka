$ErrorActionPreference = "Stop"

Write-Host "============================================================"
Write-Host " KROKIET - DUPLICATE ENGINE SELECTOR VALIDATION"
Write-Host "============================================================"
Write-Host ""

Write-Host "[1/4] Fast build check"
.\kdev.ps1 check -FastEngine
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ""
Write-Host "[2/4] Fast GUI bridge / selector tests"
.\kdev.ps1 test connect_scan::fast_duplicate_gui::tests:: -FastEngine
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ""
Write-Host "[3/4] Full Fast Engine test suite"
.\kdev.ps1 test -FastEngine
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ""
Write-Host "[4/4] Legacy build check (Fast feature disabled)"
.\kdev.ps1 check
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ""
Write-Host "ENGINE SELECTOR VALIDATION: OK" -ForegroundColor Green
