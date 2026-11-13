$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

Push-Location $PSScriptRoot
try {
    Write-Host '============================================================'
    Write-Host ' KROKIET 4B.6 + 4B.7 - COOPERATIVE CALIBRATION VALIDATION'
    Write-Host '============================================================'

    $paths = @(
        'krokiet/src/duplicate_engine/tuning.rs',
        'krokiet/src/duplicate_engine/mod.rs',
        'krokiet/src/connect_scan.rs',
        'krokiet/ui/popups/popup_fast_calibration.slint'
    )
    foreach ($path in $paths) {
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "Missing file: $path"
        }
    }
    if (-not (Test-Path -LiteralPath '.\kdev.ps1' -PathType Leaf)) {
        throw 'Missing kdev.ps1 in repository root'
    }

    $tuning = Get-Content -LiteralPath $paths[0] -Raw -Encoding UTF8
    $module = Get-Content -LiteralPath $paths[1] -Raw -Encoding UTF8
    $scan = Get-Content -LiteralPath $paths[2] -Raw -Encoding UTF8
    $popup = Get-Content -LiteralPath $paths[3] -Raw -Encoding UTF8

    $checks = @(
        @{ Name = 'Observer implementation'; Source = $tuning; Needle = 'pub(crate) fn with_calibration_ui_control' },
        @{ Name = 'Checkpoint inside verifier window'; Source = $tuning; Needle = 'while verification_runs == 0 || measured < target_sample_time {' },
        @{ Name = 'Cancellation interrupt'; Source = $tuning; Needle = 'io::ErrorKind::Interrupted' },
        @{ Name = 'Progress from measured verifier windows'; Source = $tuning; Needle = 'calibration_ui_measured_window();' },
        @{ Name = 'Safe persistence checkpoint'; Source = $tuning; Needle = 'calibration_ui_progress(96, "Guardando perfil");' },
        @{ Name = 'Observer public to GUI'; Source = $module; Needle = 'with_calibration_ui_control,' },
        @{ Name = 'Observer wired to GUI'; Source = $scan; Needle = 'let calibration_result = with_calibration_ui_control(' },
        @{ Name = 'Stage progress publishing'; Source = $scan; Needle = 'fn publish_fast_calibration_stage_progress(' },
        @{ Name = 'Cancellation reported separately'; Source = $scan; Needle = 'Err(error) if error.kind() == std::io::ErrorKind::Interrupted' },
        @{ Name = 'Progress popup callback'; Source = $popup; Needle = 'update_popup(status, detail, progress) =>' }
    )
    foreach ($check in $checks) {
        if (-not $check.Source.Contains($check.Needle)) {
            throw "Failed code check: $($check.Name)"
        }
        Write-Host "[OK] $($check.Name)"
    }
    if ($popup.Contains('timer := Timer')) {
        throw 'The calibration popup still contains fake time-based progress'
    }

    function Invoke-KrokietValidation([string]$label, [string[]]$arguments) {
        Write-Host ''
        Write-Host "--- $label ---"
        & .\kdev.ps1 @arguments
        if ($LASTEXITCODE -ne 0) {
            throw "$label failed: exit $LASTEXITCODE"
        }
    }

    Invoke-KrokietValidation '1/4 Fast Engine check' @('check', '-FastEngine')
    Invoke-KrokietValidation '2/4 Cooperative observer tests' @('test', 'cooperative_ui_tests::', '-FastEngine')
    Invoke-KrokietValidation '3/4 Fast Engine test suite' @('test', '-FastEngine')
    Invoke-KrokietValidation '4/4 Legacy check' @('check')

    Write-Host ''
    Write-Host 'KROKIET 4B.6 + 4B.7 - VALIDATION OK'
}
finally {
    Pop-Location
}
