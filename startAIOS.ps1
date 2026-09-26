# ============================================================
#  startAIOS.ps1 — Synapse AI-OS 一键构建 + QEMU 运行 + 结果判定
#
#  用法:
#    .\startAIOS.ps1              # 构建 + QEMU 运行 + 自动判定成功/失败
#    .\startAIOS.ps1 -BuildOnly   # 只构建镜像，不运行
#    .\startAIOS.ps1 -Test        # 运行 P1-T8 QEMU 测试套件 (4 用例)
#    .\startAIOS.ps1 -Tail 30     # 判定后多打印几行串口日志 (默认 15)
#
#  判定标准 (xtask run 自身永远 exit 1，不可作为依据):
#    1. QEMU 退出码 = 363  ((0xB5<<1)|1, isa-debug-exit 正常收尾)
#    2. logs\serial.log 出现 "N/N checks passed" 且无 [PANIC]
#    3. logs\debugcon-kernel.log boot marker 序列完整 (打印供人工核对)
# ============================================================
param(
    [switch]$BuildOnly,
    [switch]$Test,
    [int]$Tail = 15
)

# 注意: 用 Continue 而非 Stop —— PS 5.1 下 cargo/python 写 stderr（编译警告等）
# 一旦被重定向就会变成 NativeCommandError 误中断脚本；成败判定全部走 $LASTEXITCODE。
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root

$serialLog = Join-Path $root 'logs\serial.log'
$dbg501Log = Join-Path $root 'logs\debugcon-kernel.log'

function Write-Step($msg)  { Write-Host "`n==> $msg" -ForegroundColor Cyan }
function Write-Ok($msg)    { Write-Host "[ OK ] $msg" -ForegroundColor Green }
function Write-Bad($msg)   { Write-Host "[FAIL] $msg" -ForegroundColor Red }
function Write-Note($msg)  { Write-Host "       $msg" -ForegroundColor DarkGray }

# ---------- 前置检查 ----------
Write-Step '[0/3] 环境检查'
foreach ($tool in @('cargo', 'qemu-system-x86_64', 'python')) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        Write-Bad "找不到 $tool，请确认已安装并在 PATH 中"; exit 1
    }
}
New-Item -ItemType Directory -Force -Path (Join-Path $root 'logs') | Out-Null
Write-Ok 'cargo / qemu / python 就绪'

# ---------- 构建 ----------
Write-Step '[1/3] 构建内核镜像 (xtask build)'
cargo run -p synapse-xtask -- build
if ($LASTEXITCODE -ne 0) {
    Write-Bad '构建失败 — 请检查上方编译错误'; exit 1
}
$img = Join-Path $root 'kernel_hd.img'
Write-Ok ("kernel_hd.img 已生成 ({0:N0} 字节)" -f (Get-Item $img).Length)
if ($BuildOnly) { Write-Ok '-BuildOnly 完成'; exit 0 }

# ---------- 测试套件模式 ----------
if ($Test) {
    Write-Step '[2/3] 运行 P1-T8 QEMU 测试套件'
    python (Join-Path $root 'kernel\tests\run_tests.py')
    if ($LASTEXITCODE -eq 0) { Write-Ok '测试套件 PASS'; exit 0 }
    else { Write-Bad '测试套件 FAIL'; exit 1 }
}

# ---------- QEMU 运行 ----------
Write-Step '[2/3] QEMU 无头运行'
# 清理旧日志，避免读到上一次运行的内容
Remove-Item $serialLog, $dbg501Log -ErrorAction SilentlyContinue

# PS 5.1: 原生命令 stderr 经 2>&1 进管道会被当作 ErrorRecord，
# 配合 EAP=Stop 会误抛 NativeCommandError — 临时降级再恢复。
$ErrorActionPreference = 'Continue'
$runOut = (cargo run -p synapse-xtask -- run 2>&1 | Out-String)
$ErrorActionPreference = 'Stop'
$runOut -split "`n" | Where-Object { $_ -match '\[xtask\]' } | ForEach-Object { Write-Note $_.Trim() }

$qemuExit = -1
if ($runOut -match 'QEMU exited with exit code: (\d+)') { $qemuExit = [int]$Matches[1] }

# ---------- 结果判定 ----------
Write-Step '[3/3] 结果判定'
$fail = @()

# 1) QEMU 退出码
if ($qemuExit -eq 363) { Write-Ok 'QEMU 退出码 = 363 (正常收尾)' }
else { Write-Bad "QEMU 退出码 = $qemuExit (期望 363)"; $fail += 'exit-code' }

# 2) 串口日志: smoke 全通过 + 无 panic
if (Test-Path $serialLog) {
    $serial = Get-Content $serialLog -Raw
    if ($serial -match '(\d+)/\1 checks passed') { Write-Ok "smoke 测试: $($Matches[0])" }
    else { Write-Bad 'serial.log 未出现 "N/N checks passed"'; $fail += 'smoke' }
    if ($serial -match '\[PANIC\]') { Write-Bad '检测到 [PANIC]，回溯见 serial.log'; $fail += 'panic' }
    else { Write-Ok '无 [PANIC]' }
} else { Write-Bad 'serial.log 不存在 (QEMU 未正常启动?)'; $fail += 'no-serial' }

# 3) boot marker (打印供核对, 序列会随新任务增长)
if (Test-Path $dbg501Log) {
    $markers = (Get-Content $dbg501Log -Raw).Trim()
    Write-Ok "boot markers: $markers"
    if ($markers.Length -lt 5) { Write-Bad 'boot marker 过短，早期引导即崩溃'; $fail += 'markers' }
} else { Write-Bad 'debugcon-kernel.log 不存在'; $fail += 'no-dbg501' }

# ---------- 串口日志摘要 ----------
if (Test-Path $serialLog) {
    Write-Host "`n----- serial.log (末尾 $Tail 行) -----" -ForegroundColor Yellow
    Get-Content $serialLog -Tail $Tail | ForEach-Object { Write-Host "  $_" }
    Write-Host '--------------------------------------' -ForegroundColor Yellow
}

if ($fail.Count -eq 0) {
    Write-Host "`n=== Synapse AI-OS 运行成功 ===" -ForegroundColor Green
    exit 0
} else {
    Write-Host "`n=== 运行失败: $($fail -join ', ') — 详见 logs\ 目录 ===" -ForegroundColor Red
    exit 1
}
