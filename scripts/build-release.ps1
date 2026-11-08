# Gera a versão final otimizada e o instalador em dist\Acervo-Setup.exe.
#   powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot -Parent)
$env:Path += ";$env:USERPROFILE\.cargo\bin"

# 1) Ícone (.ico) a partir do SVG, se ainda não existir
if (-not (Test-Path assets\acervo.ico)) { cargo run --example make_icon; if ($LASTEXITCODE) { exit $LASTEXITCODE } }

# 2) Programas otimizados (o instalador, nesta rodada, ainda sai com a carga vazia)
cargo build --release --bin acervo --bin acervo-cli --bin acervo-service --bin acervo-tray
if ($LASTEXITCODE) { exit $LASTEXITCODE }

# 3) Copia os programas para a carga do instalador e compila o instalador com eles dentro
New-Item -ItemType Directory -Force payload, dist | Out-Null
foreach ($name in 'acervo.exe', 'acervo-service.exe', 'acervo-cli.exe', 'acervo-tray.exe') { Copy-Item "target\release\$name" "payload\$name" -Force }
cargo build --release --bin acervo-setup
if ($LASTEXITCODE) { exit $LASTEXITCODE }
# O nome do instalador leva a versão do Cargo.toml
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$installer = "dist\Acervo-Setup-$version.exe"
Copy-Item target\release\acervo-setup.exe $installer -Force

Get-ChildItem $installer | Select-Object Name, @{n = 'MB'; e = { [math]::Round($_.Length / 1MB, 1) } }
