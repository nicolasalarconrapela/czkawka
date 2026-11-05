param()

$ErrorActionPreference = 'Stop'

Write-Host '============================================================'
Write-Host ' KROKIET - CALIBRATION PROFILE SAFETY VALIDATION'
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

$checks = @(
    @{ Data = $tools; Value = 'enabled: root.has_saved_fast_profile;'; Name = 'Delete button disabled without matching profile' },
    @{ Data = $tools; Value = 'Callabler.has_fast_engine_calibration()'; Name = 'Button state refreshed' },
    @{ Data = $callabler; Value = 'callback has_fast_engine_calibration() -> bool;'; Name = 'Availability callback declared' },
    @{ Data = $scan; Value = 'on_has_fast_engine_calibration'; Name = 'Rust checks persisted profile availability' },
    @{ Data = $scan; Value = 'saved_fast_storage_keys(&settings.included_paths)'; Name = 'Only persisted included profiles considered' },
    @{ Data = $scan; Value = 'pending_preview.lock()'; Name = 'Confirmation snapshot stored' },
    @{ Data = $scan; Value = 'pending_confirm.lock()'; Name = 'Confirmation required before deletion' },
    @{ Data = $scan; Value = 'keys.contains(&profile.storage_key)'; Name = 'Only selected volumes deleted' },
    @{ Data = $scan; Value = 'retained.upsert(profile.clone())'; Name = 'Other volumes preserved' },
    @{ Data = $deletePopup; Value = 'text: "Cancelar";'; Name = 'Confirmation can be rejected' },
    @{ Data = $deletePopup; Value = 'text: "Eliminar perfiles";'; Name = 'Explicit destructive confirmation' },
    @{ Data = $window; Value = 'Callabler.confirm_delete_fast_engine_calibration()'; Name = 'Confirmation wired in main window' }
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

Write-Host 'CALIBRATION PROFILE SAFETY VALIDATION: OK'
