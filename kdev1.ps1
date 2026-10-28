[CmdletBinding(PositionalBinding = $false)]
param(
    [Parameter(Position = 0)]
    [ValidateSet(
        "run",
        "build",
        "check",

        "fast-release",
        "release",

        "test",
        "test-ci",
        "test-all",


        "timings",
        "timings-release",

        "stats",
        "zero-stats",
        "cache-path"
    )]
    [string]$Command = "run",

    # Optional Cargo test filter. This is the ONLY second positional argument.
    # Example: .\kdev.ps1 test duplicate_engine::tests::exact_verifier -FastEngine
    # The filter is passed to the test runner while keeping the same Cargo
    # package/features/profile, which lets Cargo/sccache reuse the compiled
    # test binary between different test names.
    [Parameter(Position = 1)]
    [string]$TestName = "",

    # Compile/run Krokiet with the experimental Fast Engine feature.
    # Applies to run/build/check/timings and tests.
    # Keep this enabled consistently for Fast Engine work to maximize cache reuse.
    [switch]$FastEngine,

    # Benchmarks should use the release test profile. This is a different
    # compilation cache from the normal test profile by design.
    [switch]$TestRelease,

    # Run ignored tests such as visual benchmarks.
    [switch]$Ignored,

    # Optional benchmark environment used when TestName selects one of the
    # Fast Engine benchmark tests. Because PositionalBinding is disabled, all
    # parameters below are named-only and can never steal the TestName argument.
    [ValidateRange(1, 16)]
    [int]$BenchScale = 1,

    [ValidateRange(1, 15)]
    [int]$BenchRuns = 3,

    [string]$BenchPath = ""
)

$ErrorActionPreference = "Stop"


# ============================================================
# KROKIET LOCAL DEVELOPMENT SCRIPT
# ============================================================
#
# Todo lo específico de nuestra máquina vive aquí.
#
# NO modifica:
#
#   Cargo.toml
#   .cargo/config.toml
#
# El script genera una configuración temporal de Cargo en:
#
#   %TEMP%\krokiet-dev\cargo-local.toml
#
# y la pasa mediante:
#
#   cargo --config <archivo>
#
# ============================================================


# ============================================================
# REPOSITORY ROOT
# ============================================================

$repoRoot = $PSScriptRoot


# ============================================================
# FILE HASHING (NO Get-FileHash DEPENDENCY)
# ============================================================
#
# Some reduced/embedded PowerShell environments do not expose the
# Microsoft.PowerShell.Utility Get-FileHash cmdlet. Use .NET directly so
# Cargo.lock protection works independently of that module.
#
# ============================================================

function Get-KrokietSha256 {
    param(
        [Parameter(Mandatory = $true)]
        [string]$LiteralPath
    )

    $stream = [System.IO.File]::OpenRead($LiteralPath)
    $sha256 = [System.Security.Cryptography.SHA256]::Create()

    try {
        $hashBytes = $sha256.ComputeHash($stream)
        return ([System.BitConverter]::ToString($hashBytes)).Replace("-", "")
    }
    finally {
        $sha256.Dispose()
        $stream.Dispose()
    }
}


# ============================================================
# PLATFORM
# ============================================================

$isWindowsPlatform = $env:OS -eq "Windows_NT"


# ============================================================
# VALIDATE CARGO
# ============================================================

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "Cargo no está instalado o no está disponible en PATH."
    exit 1
}


# ============================================================
# VALIDATE SCCACHE
# ============================================================

if (-not (Get-Command sccache -ErrorAction SilentlyContinue)) {
    Write-Error "sccache no está instalado o no está disponible en PATH."
    exit 1
}


# ============================================================
# SCCACHE CONFIGURATION
# ============================================================
#
# La caché queda FUERA del repositorio.
#
# Esto evita:
#
#   debug/
#   cache/
#   artefactos extra
#
# dentro del Git checkout.
#
# ============================================================

if (-not $env:SCCACHE_DIR) {

    if ($env:LOCALAPPDATA) {
        $env:SCCACHE_DIR = Join-Path $env:LOCALAPPDATA "sccache"
    }
    else {
        $env:SCCACHE_DIR = Join-Path $HOME ".cache\sccache"
    }
}


# Tamaño máximo de caché local.
#
# Si ya tienes SCCACHE_CACHE_SIZE configurado
# globalmente, respetamos ese valor.

if (-not $env:SCCACHE_CACHE_SIZE) {
    $env:SCCACHE_CACHE_SIZE = "20G"
}


# Crear directorio si todavía no existe.

New-Item `
    -ItemType Directory `
    -Force `
    -Path $env:SCCACHE_DIR `
    | Out-Null


# Arrancar servidor de sccache.
#
# Si ya estaba arrancado, no es un problema.

try {
    & sccache --start-server 2>$null | Out-Null
}
catch {
    # El servidor puede estar ya activo.
}


# ============================================================
# TEMPORARY CARGO CONFIGURATION
# ============================================================

$localConfigDir = Join-Path $env:TEMP "krokiet-dev"

$localConfig = Join-Path `
    $localConfigDir `
    "cargo-local.toml"


New-Item `
    -ItemType Directory `
    -Force `
    -Path $localConfigDir `
    | Out-Null


# ============================================================
# PROTECT CARGO.LOCK
# ============================================================
#
# Cargo may decide that Cargo.lock needs to be refreshed while
# running build/check/test.
#
# We allow Cargo to do that internally, but restore the exact
# original Cargo.lock before the script exits.
#
# This preserves:
#   - the tracked Cargo.lock
#   - any pre-existing local changes in Cargo.lock
#
# ============================================================

$cargoLockPath = Join-Path $repoRoot "Cargo.lock"
$cargoLockBackup = Join-Path $localConfigDir ("Cargo.lock.backup.{0}" -f $PID)

$cargoLockExisted = Test-Path -LiteralPath $cargoLockPath

if ($cargoLockExisted) {
    [System.IO.File]::Copy(
        $cargoLockPath,
        $cargoLockBackup,
        $true
    )
}
elseif (Test-Path -LiteralPath $cargoLockBackup) {
    Remove-Item -LiteralPath $cargoLockBackup -Force
}


# ============================================================
# GENERATED CARGO CONFIG
# ============================================================

$configContent = @'
# ============================================================
# GENERATED AUTOMATICALLY BY kdev.ps1
#
# DO NOT EDIT.
#
# This file is recreated every time kdev.ps1 runs.
# ============================================================


# ============================================================
# SCCACHE
# ============================================================

[build]

rustc-wrapper = "sccache"


# ============================================================
# FAST DEVELOPMENT PROFILE
# ============================================================
#
# Used by:
#
#   kdev.ps1 run
#   kdev.ps1 build
#   kdev.ps1 check
#   kdev.ps1 timings
#
# ============================================================

[profile.fastdev]

inherits = "dev"

# Development compilation should be as cheap as possible.
opt-level = 0

# Avoid large debug information.
debug = 0

# sccache is our main compiler cache.
#
# Rust incremental compilation cannot be cached by sccache.
incremental = false

# Disable LTO completely.
lto = "off"

# Maximum codegen parallelism.
codegen-units = 256


# ============================================================
# FASTDEV DEPENDENCIES
# ============================================================
#
# The upstream Cargo.toml contains:
#
# [profile.dev.package."*"]
# opt-level = 3
#
# That is useful for runtime performance, but very expensive
# for our development compilation.
#
# This local override changes dependencies back to opt-level 0.
#
# ============================================================

[profile.fastdev.package."*"]

opt-level = 0

debug = 0

codegen-units = 256


# ============================================================
# FASTDEV BUILD SCRIPTS / PROC MACROS
# ============================================================

[profile.fastdev.build-override]

opt-level = 0

debug = 0

codegen-units = 256


# ============================================================
# LOCAL FAST RELEASE
# ============================================================
#
# Upstream already defines:
#
# [profile.fast_release]
# inherits = "release"
# incremental = true
#
# Locally we disable incremental compilation so that sccache
# can cache compilable Rust crates.
#
# Everything else continues to come from the original
# fast_release profile.
#
# ============================================================

[profile.fast_release]

incremental = false


# ============================================================
# LOCAL TEST PROFILE
# ============================================================
#
# Upstream currently uses opt-level = 3 for tests.
#
# Locally we care more about test compilation speed.
#
# Keep:
#
#   debug assertions
#   overflow checks
#
# Disable:
#
#   optimization
#   incremental Rust cache
#   debug symbols
#   LTO
#
# ============================================================

[profile.test]

opt-level = 0

debug = 0

incremental = false

lto = "off"

codegen-units = 256

debug-assertions = true

overflow-checks = true


# ============================================================
# TEST DEPENDENCIES
# ============================================================

[profile.test.package."*"]

opt-level = 0

debug = 0

codegen-units = 256


# ============================================================
# TEST BUILD SCRIPTS / PROC MACROS
# ============================================================

[profile.test.build-override]

opt-level = 0

debug = 0

codegen-units = 256
'@


Set-Content `
    -LiteralPath $localConfig `
    -Value $configContent `
    -Encoding UTF8


# ============================================================
# COMMON CARGO ARGUMENTS
# ============================================================

$cargoConfigArgs = @(
    "--config",
    $localConfig
)


# ============================================================
# HELPER
# ============================================================

function Show-Header {

    param(
        [string]$Title,
        [string]$Profile = "",
        [string]$Features = ""
    )

    Write-Host ""
    Write-Host "============================================================"
    Write-Host " KROKIET - $Title"
    Write-Host "============================================================"

    if ($Profile) {
        Write-Host " Profile : $Profile"
    }

    if ($Features) {
        Write-Host " Features: $Features"
    }

    Write-Host " sccache : $env:SCCACHE_DIR"
    Write-Host ""
}


# ============================================================
# EXECUTE
# ============================================================

$exitCode = 0

Push-Location $repoRoot

try {

    switch ($Command) {


        # ====================================================
        # RUN FASTDEV
        # ====================================================

        "run" {

            $devFeatureList = @("winit_femtovg")
            if ($FastEngine) {
                $devFeatureList += "fast_duplicates"
            }
            $devFeatures = $devFeatureList -join ","

            Show-Header `
                "RUN" `
                "fastdev" `
                $devFeatures

            Write-Host " Fast Engine : $($FastEngine.IsPresent)"
            Write-Host ""

            & cargo @cargoConfigArgs run `
                -p krokiet `
                --bin krokiet `
                --profile fastdev `
                --no-default-features `
                --features $devFeatures

            $exitCode = $LASTEXITCODE
        }


        # ====================================================
        # BUILD FASTDEV
        # ====================================================

        "build" {

            $devFeatureList = @("winit_femtovg")
            if ($FastEngine) {
                $devFeatureList += "fast_duplicates"
            }
            $devFeatures = $devFeatureList -join ","

            Show-Header `
                "BUILD" `
                "fastdev" `
                $devFeatures

            Write-Host " Fast Engine : $($FastEngine.IsPresent)"
            Write-Host ""

            & cargo @cargoConfigArgs build `
                -p krokiet `
                --bin krokiet `
                --profile fastdev `
                --no-default-features `
                --features $devFeatures

            $exitCode = $LASTEXITCODE

            if ($exitCode -eq 0) {

                Write-Host ""
                Write-Host "Executable:"
                Write-Host "target\fastdev\krokiet.exe"
            }
        }


        # ====================================================
        # CHECK FASTDEV
        # ====================================================

        "check" {

            $devFeatureList = @("winit_femtovg")
            if ($FastEngine) {
                $devFeatureList += "fast_duplicates"
            }
            $devFeatures = $devFeatureList -join ","

            Show-Header `
                "CHECK" `
                "fastdev" `
                $devFeatures

            Write-Host " Fast Engine : $($FastEngine.IsPresent)"
            Write-Host ""

            & cargo @cargoConfigArgs check `
                -p krokiet `
                --bin krokiet `
                --profile fastdev `
                --no-default-features `
                --features $devFeatures

            $exitCode = $LASTEXITCODE
        }


        # ====================================================
        # FAST RELEASE
        # ====================================================
        #
        # Uses the upstream fast_release profile.
        #
        # We only override incremental=false locally so
        # that it plays better with sccache.
        #
        # Default Krokiet features are preserved.
        #
        # ====================================================

        "fast-release" {

            Show-Header `
                "FAST RELEASE" `
                "fast_release" `
                "default"

            & cargo @cargoConfigArgs build `
                -p krokiet `
                --bin krokiet `
                --profile fast_release

            $exitCode = $LASTEXITCODE

            if ($exitCode -eq 0) {

                Write-Host ""
                Write-Host "Executable:"
                Write-Host "target\fast_release\krokiet.exe"
            }
        }


        # ====================================================
        # RELEASE
        # ====================================================
        #
        # Real upstream release profile.
        #
        # We DO NOT override the release profile.
        #
        # This is the closest command to the actual project
        # production build while still using sccache as the
        # rustc wrapper.
        #
        # ====================================================

        "release" {

            Show-Header `
                "RELEASE" `
                "release" `
                "default"

            & cargo @cargoConfigArgs build `
                -p krokiet `
                --bin krokiet `
                --release

            $exitCode = $LASTEXITCODE

            if ($exitCode -eq 0) {

                Write-Host ""
                Write-Host "Executable:"
                Write-Host "target\release\krokiet.exe"
            }
        }


        # ====================================================
        # TEST / FILTERED TEST
        # ====================================================
        #
        # Without -TestName this behaves like the normal daily
        # Krokiet test command.
        #
        # With -TestName, Cargo still builds the SAME test target
        # for a given profile + feature set; the name only filters
        # which test function the test runner executes. This is the
        # preferred way to iterate over Fast Engine tests while
        # reusing Cargo artifacts and sccache.
        #
        # IMPORTANT FOR SCCACHE:
        #
        #   Use -FastEngine consistently for every Fast Engine test.
        #   Changing features changes the rustc command and therefore
        #   produces a different cache key.
        #
        # The exact verifier I/O benchmark now uses the standard Rust slice
        # comparison only; no external byte-comparison feature is required.
        #
        # Examples:
        #
        #   .\kdev.ps1 test
        #
        #   .\kdev.ps1 test `
        #       -FastEngine `
        #       -TestName "duplicate_engine::tests::exact_verifier"
        #
        #   .\kdev.ps1 test `
        #       -FastEngine `
        #       -TestName "czkawka_and_fclones_agree_on_basic_dataset"
        #
        #   .\kdev.ps1 test `
        #       -FastEngine `
        #       -TestRelease `
        #       -Ignored `
        #       -TestName "visual_duplicate_engine_benchmark" `
        #       -BenchScale 4 `
        #       -BenchRuns 3
        #
        #   .\kdev.ps1 test exact_verifier_real_dataset_benchmark `
        #       -FastEngine -TestRelease -Ignored `
        #       -BenchPath "Q:\KrokietBenchmark" -BenchRuns 9
        #
        # TestName is positional too, so this shorter form also works:
        #
        #   .\kdev.ps1 test czkawka_and_fclones_agree_on_basic_dataset -FastEngine
        #
        # ====================================================

        "test" {

            $testFeatureList = @("winit_femtovg")

            if ($FastEngine) {
                $testFeatureList += "fast_duplicates"
            }

            $testFeatures = $testFeatureList -join ","

            $testProfile = if ($TestRelease) {
                "release"
            }
            else {
                "test"
            }

            $testTitle = if ([string]::IsNullOrWhiteSpace($TestName)) {
                "FAST TEST"
            }
            else {
                "TEST: $TestName"
            }

            Show-Header `
                $testTitle `
                $testProfile `
                $testFeatures

            if (-not [string]::IsNullOrWhiteSpace($TestName)) {
                Write-Host " Test filter : $TestName"
            }
            Write-Host " Fast Engine : $($FastEngine.IsPresent)"
            Write-Host " Ignored     : $($Ignored.IsPresent)"
            Write-Host ""

            $previousBacktrace = $env:RUST_BACKTRACE
            $previousScale = $env:KROKIET_DUP_BENCH_SCALE
            $previousRuns = $env:KROKIET_DUP_BENCH_RUNS
            $previousPath = $env:KROKIET_DUP_BENCH_PATH
            $env:RUST_BACKTRACE = "1"

            $isGeneratedBenchmark = $TestName -eq "visual_duplicate_engine_benchmark"
            $isRealBenchmark = $TestName -in @(
                "duplicate_engine_real_dataset_benchmark",
                "exact_verifier_real_dataset_benchmark"
            )

            if ($TestName -eq "exact_verifier_real_dataset_benchmark") {
                if (-not $FastEngine) {
                    Write-Error "exact_verifier_real_dataset_benchmark requiere -FastEngine."
                    $exitCode = 2
                    break
                }
            }

            if ($isGeneratedBenchmark) {
                $env:KROKIET_DUP_BENCH_SCALE = $BenchScale.ToString()
                $env:KROKIET_DUP_BENCH_RUNS = $BenchRuns.ToString()
                Remove-Item Env:KROKIET_DUP_BENCH_PATH -ErrorAction SilentlyContinue

                Write-Host " Benchmark scale: $BenchScale"
                Write-Host " Benchmark runs : $BenchRuns"
                Write-Host ""
            }
            elseif ($isRealBenchmark) {
                if ([string]::IsNullOrWhiteSpace($BenchPath)) {
                    Write-Error "El test $TestName requiere -BenchPath."
                    $exitCode = 2
                    break
                }

                if (-not (Test-Path -LiteralPath $BenchPath -PathType Container)) {
                    Write-Error "La carpeta de benchmark no existe: $BenchPath"
                    $exitCode = 2
                    break
                }

                $resolvedBenchPath = (Resolve-Path -LiteralPath $BenchPath).Path
                $env:KROKIET_DUP_BENCH_RUNS = $BenchRuns.ToString()
                $env:KROKIET_DUP_BENCH_PATH = $resolvedBenchPath
                Remove-Item Env:KROKIET_DUP_BENCH_SCALE -ErrorAction SilentlyContinue

                Write-Host " Benchmark path: $resolvedBenchPath"
                Write-Host " Benchmark runs: $BenchRuns"
                Write-Host " Mode          : READ ONLY"
                Write-Host ""
            }

            try {

                $cargoTestArgs = @(
                    "test",
                    "-p", "krokiet",

                    # Run only Krokiet's primary test harness. Without this Cargo
                    # also launches the standalone diagnostic binaries, which have
                    # no unit tests and make filtered validation output misleading.
                    "--bin", "krokiet",

                    "--no-default-features",
                    "--features", $testFeatures
                )

                # One compiler job is safer for the Fast Engine branch on the
                # low-memory Windows development machine. It does not change
                # the sccache key.
                if ($FastEngine) {
                    $cargoTestArgs += @("-j", "1")
                }

                if ($TestRelease) {
                    $cargoTestArgs += "--release"
                }

                # Cargo's test filter is NOT part of the rustc compilation.
                # It is passed to the produced test executable, so switching
                # TestName keeps the same compiled test artifact reusable.
                if (-not [string]::IsNullOrWhiteSpace($TestName)) {
                    $cargoTestArgs += $TestName
                }

                $testRunnerArgs = @("--nocapture")

                if ($FastEngine -or -not [string]::IsNullOrWhiteSpace($TestName)) {
                    $testRunnerArgs += "--test-threads=1"
                }

                if ($Ignored) {
                    $testRunnerArgs += "--ignored"
                }

                # Capture the test harness output while still displaying it.
                # Cargo considers "0 tests matched" a successful process, which is
                # dangerous for filtered validation. We therefore require at least
                # one actually executed/passed test for this command.
                $testLog = Join-Path $localConfigDir ("krokiet-test-{0}.log" -f $PID)
                Remove-Item -LiteralPath $testLog -Force -ErrorAction SilentlyContinue

                # IMPORTANT: do not merge stderr into the PowerShell pipeline here.
                # Windows PowerShell 5.1 turns redirected native stderr into ErrorRecord
                # objects; with $ErrorActionPreference = "Stop" that aborts on normal
                # Cargo progress such as "Compiling ...". Cargo/test stdout contains
                # the harness summary we need for the zero-test guard.
                & cargo @cargoConfigArgs @cargoTestArgs -- @testRunnerArgs |
                    Tee-Object -FilePath $testLog

                $cargoTestExitCode = $LASTEXITCODE
                $exitCode = $cargoTestExitCode

                if ($cargoTestExitCode -eq 0) {
                    $testOutput = if (Test-Path -LiteralPath $testLog) {
                        Get-Content -LiteralPath $testLog -Raw
                    }
                    else {
                        ""
                    }

                    $resultMatches = [regex]::Matches(
                        $testOutput,
                        'test result:\s+ok\.\s+(\d+)\s+passed;'
                    )

                    $passedTests = 0
                    foreach ($match in $resultMatches) {
                        $passedTests += [int]$match.Groups[1].Value
                    }

                    if ($passedTests -le 0) {
                        Write-Host ""
                        Write-Host "============================================================" -ForegroundColor Red
                        Write-Host " KROKIET TEST ERROR: ZERO TESTS EXECUTED" -ForegroundColor Red
                        Write-Host "============================================================" -ForegroundColor Red
                        if (-not [string]::IsNullOrWhiteSpace($TestName)) {
                            Write-Host " Filter : $TestName" -ForegroundColor Red
                        }
                        Write-Host " Cargo returned success, but no test actually ran." -ForegroundColor Red
                        Write-Host " Validation is rejected to avoid a false positive." -ForegroundColor Red
                        Write-Host "============================================================" -ForegroundColor Red
                        $exitCode = 3
                    }
                    else {
                        Write-Host ""
                        Write-Host " Tests executed : $passedTests" -ForegroundColor Green
                    }
                }

                Remove-Item -LiteralPath $testLog -Force -ErrorAction SilentlyContinue
            }
            finally {

                $env:RUST_BACKTRACE = $previousBacktrace

                if ($isGeneratedBenchmark -or $isRealBenchmark) {
                    if ($null -eq $previousScale) {
                        Remove-Item Env:KROKIET_DUP_BENCH_SCALE -ErrorAction SilentlyContinue
                    }
                    else {
                        $env:KROKIET_DUP_BENCH_SCALE = $previousScale
                    }

                    if ($null -eq $previousRuns) {
                        Remove-Item Env:KROKIET_DUP_BENCH_RUNS -ErrorAction SilentlyContinue
                    }
                    else {
                        $env:KROKIET_DUP_BENCH_RUNS = $previousRuns
                    }

                    if ($null -eq $previousPath) {
                        Remove-Item Env:KROKIET_DUP_BENCH_PATH -ErrorAction SilentlyContinue
                    }
                    else {
                        $env:KROKIET_DUP_BENCH_PATH = $previousPath
                    }
                }
            }
        }


        # ====================================================
        # CI-LIKE TEST
        # ====================================================
        #
        # Main Windows CI equivalent:
        #
        # cargo test -p czkawka_core -p krokiet
        #
        # Default Krokiet features are enabled.
        #
        # ====================================================

        "test-ci" {

            Show-Header `
                "CI TEST" `
                "test" `
                "default"

            $previousBacktrace = $env:RUST_BACKTRACE
            $env:RUST_BACKTRACE = "1"

            try {

                & cargo @cargoConfigArgs test `
                    -p czkawka_core `
                    -p krokiet `
                    -- `
                    --nocapture

                $exitCode = $LASTEXITCODE
            }
            finally {

                $env:RUST_BACKTRACE = $previousBacktrace
            }
        }


        # ====================================================
        # FULL TEST
        # ====================================================
        #
        # Windows:
        #
        #   The workspace also contains czkawka_gui, which
        #   depends on GTK/GLib and needs pkg-config + native
        #   GTK development libraries.
        #
        #   The official Windows CI intentionally tests only:
        #
        #       czkawka_core
        #       krokiet
        #
        # Linux/macOS:
        #
        #   Run the complete workspace test suite.
        #
        # ====================================================

        "test-all" {

            $previousBacktrace = $env:RUST_BACKTRACE
            $env:RUST_BACKTRACE = "1"

            try {

                if ($isWindowsPlatform) {

                    Show-Header `
                        "WINDOWS FULL TEST" `
                        "test" `
                        "czkawka_core + krokiet"

                    Write-Host "Windows detected."
                    Write-Host "Skipping czkawka_gui because it requires GTK/GLib native dependencies."
                    Write-Host ""

                    & cargo @cargoConfigArgs test `
                        -p czkawka_core `
                        -p krokiet `
                        -- `
                        --nocapture

                    $exitCode = $LASTEXITCODE
                }
                else {

                    Show-Header `
                        "FULL WORKSPACE TEST" `
                        "test" `
                        "workspace"

                    & cargo @cargoConfigArgs test `
                        --workspace `
                        -- `
                        --nocapture

                    $exitCode = $LASTEXITCODE
                }
            }
            finally {

                $env:RUST_BACKTRACE = $previousBacktrace
            }
        }


        # ====================================================
        # FASTDEV TIMINGS
        # ====================================================

        "timings" {

            $devFeatureList = @("winit_femtovg")
            if ($FastEngine) {
                $devFeatureList += "fast_duplicates"
            }
            $devFeatures = $devFeatureList -join ","

            Show-Header `
                "FASTDEV TIMINGS" `
                "fastdev" `
                $devFeatures

            Write-Host " Fast Engine : $($FastEngine.IsPresent)"
            Write-Host ""

            & cargo @cargoConfigArgs build `
                -p krokiet `
                --bin krokiet `
                --profile fastdev `
                --no-default-features `
                --features $devFeatures `
                --timings

            $exitCode = $LASTEXITCODE

            if ($exitCode -eq 0) {

                Write-Host ""
                Write-Host "Cargo timing report:"
                Write-Host "target\cargo-timings\cargo-timing.html"
            }
        }


        # ====================================================
        # RELEASE TIMINGS
        # ====================================================

        "timings-release" {

            Show-Header `
                "RELEASE TIMINGS" `
                "release" `
                "default"

            & cargo @cargoConfigArgs build `
                -p krokiet `
                --bin krokiet `
                --release `
                --timings

            $exitCode = $LASTEXITCODE

            if ($exitCode -eq 0) {

                Write-Host ""
                Write-Host "Cargo timing report:"
                Write-Host "target\cargo-timings\cargo-timing.html"
            }
        }


        # ====================================================
        # SCCACHE STATS
        # ====================================================

        "stats" {

            Show-Header "SCCACHE STATS"

            & sccache --show-stats

            $exitCode = $LASTEXITCODE
        }


        # ====================================================
        # RESET SCCACHE STATISTICS
        # ====================================================
        #
        # Does NOT delete cached compilation objects.
        #
        # Only resets counters.
        #
        # ====================================================

        "zero-stats" {

            Show-Header "RESET SCCACHE STATS"

            & sccache --zero-stats

            $exitCode = $LASTEXITCODE
        }


        # ====================================================
        # SHOW CACHE PATH
        # ====================================================

        "cache-path" {

            Write-Host ""
            Write-Host "SCCACHE_DIR:"
            Write-Host $env:SCCACHE_DIR

            Write-Host ""
            Write-Host "SCCACHE_CACHE_SIZE:"
            Write-Host $env:SCCACHE_CACHE_SIZE

            Write-Host ""
            Write-Host "Temporary Cargo config:"
            Write-Host $localConfig

            $exitCode = 0
        }
    }
}
finally {

    # ========================================================
    # RESTORE CARGO.LOCK
    # ========================================================

    try {
        if ($cargoLockExisted) {

            if (Test-Path -LiteralPath $cargoLockBackup) {

                $restoreCargoLock = $true

                if (Test-Path -LiteralPath $cargoLockPath) {

                    $currentHash = Get-KrokietSha256 `
                        -LiteralPath $cargoLockPath

                    $backupHash = Get-KrokietSha256 `
                        -LiteralPath $cargoLockBackup

                    $restoreCargoLock = $currentHash -ne $backupHash
                }

                if ($restoreCargoLock) {

                    [System.IO.File]::Copy(
                        $cargoLockBackup,
                        $cargoLockPath,
                        $true
                    )

                    Write-Host ""
                    Write-Host "Cargo.lock was changed by Cargo and has been restored."
                }
            }
        }
        else {
            if (Test-Path -LiteralPath $cargoLockPath) {

                Remove-Item -LiteralPath $cargoLockPath -Force

                Write-Host ""
                Write-Host "Cargo generated Cargo.lock; the generated file was removed."
            }
        }
    }
    finally {

        if (Test-Path -LiteralPath $cargoLockBackup) {
            Remove-Item -LiteralPath $cargoLockBackup -Force
        }

        Pop-Location
    }
}


exit $exitCode