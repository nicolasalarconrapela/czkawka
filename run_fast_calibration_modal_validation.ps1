param()

$ErrorActionPreference = "Stop"

Write-Host ""
Write-Host "============================================================"
Write-Host " KROKIET - FAST CALIBRATION MODAL VALIDATION"
Write-Host "============================================================"
Write-Host ""

$required = @(
    ".\krokiet\src\connect_scan.rs",
    ".\krokiet\ui\main_window.slint",
    ".\krokiet\ui\popups\popup_fast_calibration.slint"
)

foreach ($file in $required) {
    if (-not (Test-Path $file)) {
        throw "Missing $file"
    }
}

$scan = Get-Content ".\krokiet\src\connect_scan.rs" -Raw

if ($scan -notmatch 'let storage_key = storage_key\.to_string\(\);') {
    throw "Invalid connect_scan.rs: storage_key must be owned before upgrade_in_event_loop"
}
if ($scan -match 'storage_key\.to_string\(\)\.into\(\),\s*\r?\n\s*overall_percent') {
    throw "Invalid connect_scan.rs: borrowed storage_key is converted inside the event-loop closure"
}

Write-Host "[OK] Calibration storage key is owned before event-loop callback"
$popup = Get-Content ".\krokiet\ui\popups\popup_fast_calibration.slint" -Raw
$window = Get-Content ".\krokiet\ui\main_window.slint" -Raw

if ($window -match 'popup_sort_results\.slint') {
    throw "Invalid main_window.slint: popup_sort_results.slint does not exist; expected popup_sort.slint"
}
if ($window -notmatch 'popups/popup_sort\.slint') {
    throw "Invalid main_window.slint: missing original popup_sort.slint import"
}
if ($popup -match 'popup_width:\s*min\(520px,\s*parent\.width') {
    throw "Invalid popup_fast_calibration.slint: root property cannot access parent.width"
}
if ($popup -notmatch 'popup_width:\s*min\(520px,\s*root\.width') {
    throw "Invalid popup_fast_calibration.slint: expected root.width for responsive popup width"
}

Write-Host "[OK] Original popup_sort.slint import preserved"
Write-Host "[OK] Calibration popup width uses root.width"

$checks = @(
    @{ Text = $scan; Pattern = "invoke_show_fast_calibration_popup"; Label = "Rust opens calibration modal" },
    @{ Text = $scan; Pattern = "invoke_update_fast_calibration_popup"; Label = "Rust updates calibration modal" },
    @{ Text = $scan; Pattern = "invoke_hide_fast_calibration_popup"; Label = "Rust closes calibration modal" },
    @{ Text = $popup; Pattern = "PopupClosePolicy.no-auto-close"; Label = "Modal cannot auto-close" },
    @{ Text = $popup; Pattern = "ProgressIndicator"; Label = "Modal has progress indicator" },
    @{ Text = $window; Pattern = 'import \{ PopupFastCalibration \}'; Label = "Main window imports calibration popup" },
    @{ Text = $window; Pattern = "callback show_fast_calibration_popup"; Label = "Main window exposes show callback" },
    @{ Text = $window; Pattern = "fast_calibration_popup_window := PopupFastCalibration"; Label = "Main window instantiates calibration popup" },
    @{ Text = $window; Pattern = "fast_calibration_popup_window.show_popup"; Label = "Main window wires show handler" },
    @{ Text = $window; Pattern = "fast_calibration_popup_window.update_popup"; Label = "Main window wires update handler" },
    @{ Text = $window; Pattern = "fast_calibration_popup_window.close_popup"; Label = "Main window wires close handler" }
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
Write-Host "FAST CALIBRATION MODAL VALIDATION: OK"
