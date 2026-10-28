$ErrorActionPreference = "Stop"

$repoRoot = $PSScriptRoot
$localConfig = Join-Path $env:TEMP "krokiet-dev\cargo-local.toml"

if (-not (Test-Path -LiteralPath $localConfig)) {
    throw @"
No existe la configuracion local generada por kdev1.ps1:
$localConfig

Ejecuta primero:
  .\run_fast_gui_activation_validation.ps1

Eso genera la configuracion local y valida la integracion.
"@
}

Push-Location $repoRoot
try {
    & cargo --config $localConfig run `
        -p krokiet `
        --bin krokiet `
        --profile fastdev `
        --no-default-features `
        --features "winit_femtovg,fast_duplicates"

    exit $LASTEXITCODE
}
finally {
    Pop-Location
}
