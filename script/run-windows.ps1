# One-shot Windows launch for this fork:
#   1) vcvars64
#   2) prefer rustup cargo + MSVC link.exe
#   3) cd repo root
#   4) ZED_STATELESS=1
#   5) cargo run -p zed
#
# Usage (from anywhere):
#   powershell -ExecutionPolicy Bypass -File D:\code\zed\script\run-windows.ps1
#   .\script\run-windows.ps1
#   .\script\run-windows.ps1 --release

$ErrorActionPreference = "Stop"

$VcVars = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
if (-not (Test-Path $VcVars)) {
    throw "vcvars64.bat not found: $VcVars"
}

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$CargoBin = Join-Path $env:USERPROFILE ".cargo\bin"

$CargoArgs = @("run", "-p", "zed") + $args
$CargoArgsQuoted = (
    $CargoArgs | ForEach-Object {
        if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ }
    }
) -join " "

$Batch = @"
@echo off
call "$VcVars" || exit /b 1
set "PATH=$CargoBin;%VCToolsInstallDir%bin\Hostx64\x64;%PATH%"
cd /d "$RepoRoot" || exit /b 1
set ZED_STATELESS=1
cargo $CargoArgsQuoted
exit /b %ERRORLEVEL%
"@

$TempBat = Join-Path ([System.IO.Path]::GetTempPath()) ("zed-run-windows-{0}.bat" -f [guid]::NewGuid().ToString("N"))
try {
    Set-Content -Path $TempBat -Value $Batch -Encoding Ascii
    $proc = Start-Process -FilePath $env:ComSpec -ArgumentList @("/c", "`"$TempBat`"") -NoNewWindow -Wait -PassThru
    exit $proc.ExitCode
}
finally {
    Remove-Item -LiteralPath $TempBat -ErrorAction SilentlyContinue
}
