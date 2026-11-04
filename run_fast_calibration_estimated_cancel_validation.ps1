param()

$ErrorActionPreference = "Stop"

Write-Host ""
Write-Host "============================================================"
Write-Host " KROKIET - FAST CALIBRATION ESTIMATED PROGRESS + CANCEL"
Write-Host "============================================================"
Write-Host ""

$files = @(
    ".\krokiet\src\connect_scan.rs",
    ".\krokiet\ui\main_window.slint",
    ".\krokiet\ui\globals\callabler.slint",
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
$callabler = Get-Content ".\krokiet\ui\globals\callabler.slint" -Raw
$progressPopup = Get-Content ".\krokiet\ui\popups\popup_fast_calibration.slint" -Raw
$resultPopup = Get-Content ".\krokiet\ui\popups\popup_fast_calibration_result.slint" -Raw

$checks = @(
    @{ Text = $progressPopup; Pattern = '% \(estimado\)'; Label = 'Estimated percent is shown' },
    @{ Text = $progressPopup; Pattern = 'Cancelando\.\.\.'; Label = 'Cancel button has cancelling state' },
    @{ Text = $progressPopup; Pattern = 'text: root\.cancelling'; Label = 'Cancel button state is wired' },
    @{ Text = $progressPopup; Pattern = 'cancel_requested'; Label = 'Progress popup emits cancellation' },
    @{ Text = $callabler; Pattern = 'cancel_fast_engine_calibration'; Label = 'Callabler exposes cancellation' },
    @{ Text = $window; Pattern = 'Callabler\.cancel_fast_engine_calibration'; Label = 'Main window wires cancel button' },
    @{ Text = $scan; Pattern = 'Arc::new\(AtomicBool::new\(false\)\)'; Label = 'Rust owns cancellation flag' },
    @{ Text = $scan; Pattern = 'FastCalibrationOutcome::Cancelled'; Label = 'Rust models cancelled outcome' },
    @{ Text = $scan; Pattern = 'restore_fast_calibration_store'; Label = 'Cancelled run restores previous profile' },
    @{ Text = $resultPopup; Pattern = 'Calibracion cancelada'; Label = 'Result modal reports cancellation' }
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
Write-Host "FAST CALIBRATION ESTIMATED PROGRESS + CANCEL: OK"
