[CmdletBinding()]
param(
    [string]$Python,
    [switch]$Offline,
    [switch]$SkipBuild,
    [string]$TestFilter,
    [ValidateSet('Package', 'Prepare', 'Check', 'Test', 'Clippy')]
    [string]$Mode = 'Package'
)

$ErrorActionPreference = 'Stop'

function Resolve-Python312([string]$Requested) {
    $candidates = @()
    if ($Requested) {
        $candidates += $Requested
    } else {
        # List installed runtimes without asking the Python launcher to install one.
        $launcher = Get-Command py.exe -CommandType Application -ErrorAction SilentlyContinue
        if ($launcher) {
            try {
                $installed = & $launcher.Source -0p 2>$null
                foreach ($line in $installed) {
                    if ($line -match '([A-Za-z]:\\.+\.exe)\s*$') { $candidates += $Matches[1] }
                }
            } catch { }
        }
        foreach ($name in @('python3.12.exe', 'python.exe', 'python3.exe')) {
            $candidates += @(Get-Command $name -All -CommandType Application -ErrorAction SilentlyContinue |
                Select-Object -ExpandProperty Source)
        }
        $candidates += @(
            "$env:LOCALAPPDATA\Programs\Python\Python312\python.exe",
            "$env:ProgramFiles\Python312\python.exe",
            "$env:USERPROFILE\.cache\codex-runtimes\codex-primary-runtime\dependencies\python\python.exe"
        )
    }

    $probe = 'import json, sys, sysconfig; print(json.dumps(dict(executable=sys.executable, version=list(sys.version_info[:2]), platform=sysconfig.get_platform(), implementation=sys.implementation.name)))'
    foreach ($candidate in ($candidates | Select-Object -Unique)) {
        $command = Get-Command $candidate -CommandType Application -ErrorAction SilentlyContinue
        if (-not $command) { continue }
        # These aliases can open Microsoft Store instead of running Python.
        if ($command.Source -match '\\Microsoft\\WindowsApps\\python(?:3(?:\.12)?)?\.exe$') { continue }
        try {
            $output = & $command.Source -I -c $probe 2>$null
            if ($LASTEXITCODE -ne 0) { continue }
            $runtime = ($output -join "`n") | ConvertFrom-Json -ErrorAction Stop
            if ($runtime.implementation -eq 'cpython' -and
                ($runtime.version -join '.') -eq '3.12' -and $runtime.platform -eq 'win-amd64') {
                return $runtime.executable
            }
        } catch { }
    }
    if ($Requested) {
        throw "Python '$Requested' was not found or is not CPython 3.12 x64. Omit -Python to auto-detect, or specify the actual python.exe path."
    }
    throw 'CPython 3.12 x64 was not found. Install it or pass -Python with the actual python.exe path. Microsoft Store aliases are not Python installations.'
}

$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
$savedEnvironment = @{}
$environmentNames = @('PATH', 'FFMPEG_DIR', 'PKG_CONFIG', 'PKG_CONFIG_PATH',
    'PKG_CONFIG_LIBDIR', 'LIBCLANG_PATH', 'BINDGEN_EXTRA_CLANG_ARGS', 'TINY_ASSET_DIR')
foreach ($name in $environmentNames) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
try {
    if ($env:OS -ne 'Windows_NT' -or -not [Environment]::Is64BitProcess) {
        throw 'Run this script in 64-bit PowerShell on Windows.'
    }
    $Python = Resolve-Python312 $Python
    Write-Host "Using Python 3.12 x64: $Python"
    if ($env:RUSTFLAGS -or $env:CARGO_ENCODED_RUSTFLAGS -or $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS) {
        throw 'Unset Rust flag environment overrides; this build uses packaging/windows/cargo.toml.'
    }
    if (-not (Get-Command lib.exe -ErrorAction SilentlyContinue)) {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
        $vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if (-not $vs) { throw 'Visual Studio C++ x64 build tools are required.' }
        & (Join-Path $vs 'Common7\Tools\Launch-VsDevShell.ps1') -Arch amd64 -HostArch amd64 -SkipAutomaticLocation
    }
    $prepareArgs = @('packaging/windows/prepare.py')
    if ($Offline) { $prepareArgs += '--offline' }
    & $Python @prepareArgs
    if ($LASTEXITCODE -ne 0) { throw 'Native dependency preparation failed.' }
    if ($Mode -eq 'Prepare') { return }
    $paths = Get-Content 'target/windows-x86_64/paths.json' -Raw | ConvertFrom-Json
    $env:FFMPEG_DIR = $paths.sdk
    $env:PKG_CONFIG = $paths.pkg_config
    $env:PKG_CONFIG_PATH = Join-Path $paths.sdk 'lib/pkgconfig'
    $env:PKG_CONFIG_LIBDIR = $env:PKG_CONFIG_PATH
    $env:LIBCLANG_PATH = $paths.libclang
    $env:BINDGEN_EXTRA_CLANG_ARGS = '-I"' + (Join-Path $paths.sdk 'include') + '"'
    $env:TINY_ASSET_DIR = $null
    $env:PATH = (Join-Path $paths.ffmpeg 'bin') + ';' + (Join-Path $paths.msys 'bin') + ';' + $env:PATH
    $cargoArgs = @('--config', 'packaging/windows/cargo.toml')
    switch ($Mode) {
        'Check' { $cargoArgs += 'check' }
        'Test' { $cargoArgs += 'test' }
        'Clippy' { $cargoArgs += 'clippy' }
        'Package' { $cargoArgs += 'build' }
    }
    $cargoArgs += @('--locked', '--target', 'x86_64-pc-windows-msvc')
    if ($Mode -eq 'Package') { $cargoArgs += '--release' }
    else { $cargoArgs += '--workspace' }
    if ($Offline) { $cargoArgs += '--offline' }
    if ($Mode -eq 'Clippy') { $cargoArgs += @('--all-targets', '--', '-D', 'warnings') }
    if ($TestFilter) {
        if ($Mode -ne 'Test') { throw '-TestFilter requires -Mode Test.' }
        $cargoArgs += @($TestFilter, '--', '--nocapture')
    }
    if (-not $SkipBuild) {
        & cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) { throw "Cargo $Mode failed." }
    }
    if ($Mode -eq 'Package') {
        & $Python 'packaging/windows/bundle.py'
        if ($LASTEXITCODE -ne 0) { throw 'Portable bundle verification failed.' }
    }
}
finally {
    foreach ($name in $environmentNames) {
        [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process')
    }
    Pop-Location
}
