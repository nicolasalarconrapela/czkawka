param()

$ErrorActionPreference = 'Stop'

Write-Host '============================================================'
Write-Host ' KROKIET - CALIBRATION PROFILE MANAGEMENT VALIDATION'
Write-Host '============================================================'

$files = @(
    '.\krokiet\src\connect_scan.rs',
    '.\krokiet\ui\screens\tool_settings.slint',
    '.\krokiet\ui\globals\callabler.slint',
    '.\krokiet\ui\main_window.slint',
    '.\krokiet\ui\popups\popup_fast_calibration_delete.slint',
    '.\krokiet\ui\popups\popup_fast_calibration_result.slint',
    '.\krokiet\ui\popups\popup_fast_calibration.slint'
)

foreach ($file in $files) {
    if (-not (Test-Path -LiteralPath $file)) {
        throw "Missing file: $file"
    }
}

$scan = Get-Content '.\krokiet\src\connect_scan.rs' -Raw
$tools = Get-Content '.\krokiet\ui\screens\tool_settings.slint' -Raw
$callabler = Get-Content '.\krokiet\ui\globals\callabler.slint' -Raw
$window = Get-Content '.\krokiet\ui\main_window.slint' -Raw
$deletePopup = Get-Content '.\krokiet\ui\popups\popup_fast_calibration_delete.slint' -Raw
$resultPopup = Get-Content '.\krokiet\ui\popups\popup_fast_calibration_result.slint' -Raw

$checks = @(
    @{ Data = $tools; Value = 'request_delete_fast_engine_calibration()'; Name = 'Delete button' },
    @{ Data = $tools; Value = 'calibrate_fast_engine()'; Name = 'Recalibrate button' },
    @{ Data = $callabler; Value = 'callback confirm_delete_fast_engine_calibration()'; Name = 'Confirmation callback' },
    @{ Data = $window; Value = 'fast_calibration_delete_popup_window := PopupFastCalibrationDelete'; Name = 'Deletion popup in main window' },
    @{ Data = $window; Value = 'Callabler.confirm_delete_fast_engine_calibration()'; Name = 'Confirm handler wired' },
    @{ Data = $deletePopup; Value = 'confirm_requested()'; Name = 'Confirmation modal connected' },
    @{ Data = $scan; Value = 'on_confirm_delete_fast_engine_calibration'; Name = 'Rust deletion handler connected' },
    @{ Data = $scan; Value = 'keys.contains(&profile.storage_key)'; Name = 'Deletion scoped to selected volumes' },
    @{ Data = $scan; Value = 'retained.upsert(profile.clone())'; Name = 'Other volume profiles retained' },
    @{ Data = $resultPopup; Value = 'Calibracion eliminada'; Name = 'Deletion result state' }
)

foreach ($check in $checks) {
    if (-not $check.Data.Contains($check.Value)) {
        throw "Validation failed: $($check.Name)"
    }
    Write-Host "[OK] $($check.Name)"
}

Write-Host '[1/3] Fast Engine compilation'
& .\kdev.ps1 check -FastEngine
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host '[2/3] Fast Engine tests'
& .\kdev.ps1 test -FastEngine
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host '[3/3] Legacy compilation'
& .\kdev.ps1 check
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host 'CALIBRATION PROFILE MANAGEMENT VALIDATION: OK'
