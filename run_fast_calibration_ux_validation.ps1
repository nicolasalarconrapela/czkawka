param()

$ErrorActionPreference = "Stop"

Write-Host ""
Write-Host "============================================================"
Write-Host " KROKIET - FAST CALIBRATION UX VALIDATION"
Write-Host "============================================================"
Write-Host ""

$files = @(
    ".\krokiet\src\connect_scan.rs",
    ".\krokiet\ui\main_window.slint",
    ".\krokiet\ui\popups\popup_fast_calibration.slint",
    ".\krokiet\ui\popups\popup_fast_calibration_result.slint"
)

foreach ($file in $files) {
    if (-not (Test-Path $file)) {
        throw "Missing $file"
    }
}

$scan = Get-Content ".\krokiet\src\connect_scan.rs" -Raw
$window = Get-Content ".\krokiet\ui\main_window.slint" -Raw
$progressPopup = Get-Content ".\krokiet\ui\popups\popup_fast_calibration.slint" -Raw
$resultPopup = Get-Content ".\krokiet\ui\popups\popup_fast_calibration_result.slint" -Raw

$checks = @(
    @{ Text = $progressPopup; Pattern = "timer := Timer"; Label = "Progress modal has animated activity" },
    @{ Text = $progressPopup; Pattern = "visual_progress"; Label = "Progress modal has visual progress state" },
    @{ Text = $progressPopup; Pattern = "Calibrando..."; Label = "Progress modal avoids frozen 0 percent display" },
    @{ Text = $resultPopup; Pattern = "PopupFastCalibrationResult"; Label = "Result modal exists" },
    @{ Text = $resultPopup; Pattern = "Calibracion completada"; Label = "Result modal has success state" },
    @{ Text = $resultPopup; Pattern = "Calibracion fallida"; Label = "Result modal has failure state" },
    @{ Text = $window; Pattern = "show_fast_calibration_result_popup"; Label = "Main window wires result modal" },
    @{ Text = $scan; Pattern = "invoke_show_fast_calibration_result_popup"; Label = "Rust opens result modal" }
)

foreach ($check in $checks) {
    if ($check.Text -notmatch $check.Pattern) {
        throw "Validation failed: $($check.Label)"
    }

    Write-Host "[OK] $($check.Label)"
}

Write-Host ""
Write-Host "[1/3] Fast Engine build check"
& .\kdev.ps1 check -FastEngine
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

Write-Host ""
Write-Host "[2/3] Fast Engine tests"
& .\kdev.ps1 test -FastEngine
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

Write-Host ""
Write-Host "[3/3] Legacy build check"
& .\kdev.ps1 check
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

Write-Host ""
Write-Host "FAST CALIBRATION UX VALIDATION: OK"
