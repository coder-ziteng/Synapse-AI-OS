# ============================================================
#  startAIOS-GUI.ps1 - Synapse AI-OS build + QEMU with display
#
#  Usage:
#    .\startAIOS-GUI.ps1              # Build + QEMU GTK window
#    .\startAIOS-GUI.ps1 -BuildOnly   # Build only, don't run
#
#  Differences from startAIOS.ps1:
#    - Uses xtask gui subcommand (-display gtk), shows VBE framebuffer / boot animation
#    - No automatic assertion (headless assertion is done by startAIOS.ps1)
#    - Script exits naturally when QEMU window closes
# ============================================================
param(
    [switch]$BuildOnly
)

$ErrorActionPreference = 'Continue'
try {
    [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
    $OutputEncoding = [System.Text.Encoding]::UTF8
} catch { }

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root

$serialLog = Join-Path $root 'logs\serial.log'

function Write-Step($msg)  { Write-Host "`n==> $msg" -ForegroundColor Cyan }
function Write-Ok($msg)    { Write-Host "[ OK ] $msg" -ForegroundColor Green }
function Write-Bad($msg)   { Write-Host "[FAIL] $msg" -ForegroundColor Red }

# ---------- Prerequisites ----------
Write-Step '[0/2] Environment check'
foreach ($tool in @('cargo', 'qemu-system-x86_64', 'python')) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        Write-Bad "$tool not found, please install and add to PATH"; exit 1
    }
}
New-Item -ItemType Directory -Force -Path (Join-Path $root 'logs') | Out-Null
Write-Ok 'cargo / qemu / python ready'

# ---------- Build ----------
Write-Step '[1/2] Build kernel image (xtask build)'
cargo run -p synapse-xtask -- build
if ($LASTEXITCODE -ne 0) {
    Write-Bad 'Build failed - check compilation errors above'; exit 1
}
$img = Join-Path $root 'kernel_hd.img'
Write-Ok ("kernel_hd.img generated ({0:N0} bytes)" -f (Get-Item $img).Length)
if ($BuildOnly) { Write-Ok '-BuildOnly complete'; exit 0 }

# ---------- QEMU with display ----------
Write-Step '[2/2] QEMU with display (GTK)'
Remove-Item $serialLog -ErrorAction SilentlyContinue

Write-Host "`n[INFO] QEMU window launched, close window or press ESC/Q to exit`n" -ForegroundColor Yellow

cargo run -p synapse-xtask -- gui
if ($LASTEXITCODE -ne 0) {
    Write-Bad "QEMU exited abnormally (exit $LASTEXITCODE)"; exit 1
}

Write-Ok 'QEMU exited normally'

# ---------- Serial log summary ----------
if (Test-Path $serialLog) {
    Write-Host "`n----- serial.log (last 20 lines) -----" -ForegroundColor Yellow
    Get-Content $serialLog -Tail 20 -Encoding UTF8 | ForEach-Object { Write-Host "  $_" }
    Write-Host '--------------------------------------' -ForegroundColor Yellow
}
