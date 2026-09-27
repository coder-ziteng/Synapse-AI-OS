# ============================================================
#  startAIOS-GUI.ps1 - Boot Synapse AI-OS in a QEMU window
#
#  Usage:
#    .\startAIOS-GUI.ps1     # Build (release + gui_demo) + QEMU GTK window
#
#  Behavior:
#    - `xtask gui` builds the kernel with the `gui_demo` cargo feature:
#      the kernel plays the boot animation once, holds the final frame,
#      and never powers off.
#    - The QEMU window stays open until you close it manually.
#    - Serial log is still captured to logs\serial.log (bootanim traces).
#
#  For headless CI-style boot verification, use startAIOS.ps1 instead.
# ============================================================

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
Write-Step '[0/1] Environment check'
foreach ($tool in @('cargo', 'qemu-system-x86_64', 'python')) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        Write-Bad "$tool not found, please install and add to PATH"; exit 1
    }
}
New-Item -ItemType Directory -Force -Path (Join-Path $root 'logs') | Out-Null
Write-Ok 'cargo / qemu / python ready'

# ---------- Build + QEMU with display ----------
Write-Step '[1/1] Build (release + gui_demo) and launch QEMU window (GTK)'
Remove-Item $serialLog -ErrorAction SilentlyContinue

Write-Host "`n[INFO] First release build may take a few minutes (-Zbuild-std)." -ForegroundColor Yellow
Write-Host "[INFO] QEMU window plays the boot animation once (~12s), then holds; close it manually to exit.`n" -ForegroundColor Yellow

cargo run -p synapse-xtask -- gui
if ($LASTEXITCODE -ne 0) {
    Write-Bad "QEMU exited abnormally (exit $LASTEXITCODE)"; exit 1
}

Write-Ok 'QEMU exited normally (window closed)'

# ---------- Serial log summary ----------
if (Test-Path $serialLog) {
    Write-Host "`n----- serial.log (last 20 lines) -----" -ForegroundColor Yellow
    Get-Content $serialLog -Tail 20 -Encoding UTF8 | ForEach-Object { Write-Host "  $_" }
    Write-Host '--------------------------------------' -ForegroundColor Yellow
}
