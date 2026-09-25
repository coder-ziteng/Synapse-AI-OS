# verify-all.ps1 — 工具链一键验证 (P0-T5)
#
# 在全新 PowerShell 中运行（验证持久化 PATH 是否生效）：
#   powershell -NoProfile -File .\verify-all.ps1
#
# 期望：四行 version 输出 + exit 0。

$ErrorActionPreference = 'Stop'

# 读 User 级 env（通过 .NET Registry API 直接读 HKCU\Environment）
# （绕开 PowerShell 5.1 中 [Environment]::GetEnvironmentVariable('X','User') 的缓存/作用域问题）
$reg = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
$env:RUSTUP_HOME = $reg.GetValue('RUSTUP_HOME', '')
$env:CARGO_HOME  = $reg.GetValue('CARGO_HOME',  '')
$userPath        = $reg.GetValue('PATH',        '')
$reg.Close()
$machinePath     = [Environment]::GetEnvironmentVariable('PATH', 'Machine')
$env:PATH        = $userPath + ';' + $machinePath

Write-Host '=== Synapse toolchain verification ==='
Write-Host ('RUSTUP_HOME = ' + $env:RUSTUP_HOME)
Write-Host ('CARGO_HOME  = ' + $env:CARGO_HOME)
Write-Host ''

$tools = @(
  @{ name = 'rustc';     cmd = 'rustc';                  arg = '--version' },
  @{ name = 'cargo';     cmd = 'cargo';                  arg = '--version' },
  @{ name = 'bootimage'; cmd = 'bootimage';              arg = '--version' },
  @{ name = 'qemu';      cmd = 'qemu-system-x86_64.exe'; arg = '--version' }
)

$fail = 0
foreach ($t in $tools) {
  $bin = (Get-Command $t.cmd -ErrorAction SilentlyContinue).Source
  if (-not $bin) {
    Write-Host ("  [-] {0,-10} NOT FOUND in PATH" -f $t.name)
    $fail++
    continue
  }
  Write-Host ("  [+] {0,-10} {1}" -f $t.name, $bin)
  $v = & $t.cmd $t.arg 2>&1 | Select-Object -First 1
  Write-Host ("      -> {0}" -f $v)
}

Write-Host ''
if ($fail -eq 0) {
  Write-Host '=== ALL OK ==='
  exit 0
} else {
  Write-Host ("=== {0} MISSING ===" -f $fail)
  exit 1
}