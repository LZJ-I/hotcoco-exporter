$ErrorActionPreference = 'Stop'
# Reuse the installed WSL Rust target and MinGW. Never installs a toolchain or dependencies.
& wsl.exe --cd $PSScriptRoot --exec bash -lc 'cargo build --offline --release --target x86_64-pc-windows-gnu'
if ($LASTEXITCODE -ne 0) { throw 'Rust EXE build failed.' }
$exportBinary = Join-Path $PSScriptRoot 'target\x86_64-pc-windows-gnu\release\hotcoco-exporter.exe'
Get-Item -LiteralPath $exportBinary | Select-Object FullName, Length
