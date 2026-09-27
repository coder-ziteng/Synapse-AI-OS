#!/usr/bin/env python3
"""Cargo test runner — 在 QEMU 中执行裸机测试二进制（P1-T8）。

用法（cargo 通过 runner 机制自动调用）::

    # 方式 1：环境变量（不修改 .cargo/config.toml）
    CARGO_TARGET_X86_64_BOOTLOADER_RUNNER="python tests/qemu_test_runner.py" \
        cargo test -p synapse-kernel \
        --target x86_64-bootloader.json \
        -Zjson-target-spec -Zbuild-std=core,alloc,compiler_builtins

    # 方式 2：.cargo/config.toml（需另一窗口配合添加）
    [target.x86_64-bootloader]
    runner = "python tests/qemu_test_runner.py"

cargo 传入 argv[1] = 测试 ELF 路径，其余参数（测试过滤器等）忽略。

流程：
    1. build_disk.py 把 ELF 打包为可引导磁盘镜像
    2. QEMU 执行（isa-debug-exit @ 0x502，串口输出写 logs/）
    3. 退出码翻译：103（全部通过）→ 0；175（存在失败）→ 1；其他 → 2
"""

import os
import shutil
import subprocess
import sys

# 路径解析（不依赖 cwd —— cargo 以包根目录 kernel/ 为 cwd 调用 runner）
SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))   # .../kernel/tests
REPO_ROOT = os.path.dirname(os.path.dirname(SCRIPT_DIR))  # .../ai-os
BUILD_DISK = os.path.join(REPO_ROOT, "build_disk.py")
IMG_PATH = os.path.join(REPO_ROOT, "test_output", "cargo_test.img")
SERIAL_LOG = os.path.join(REPO_ROOT, "logs", "basic_boot_serial.log")

# QEMU 退出码约定（isa-debug-exit: exit = ((val & 0x7F) << 1) | 1）
EXIT_ALL_PASS = 103   # val=0x33
EXIT_HAS_FAIL = 175   # val=0x55
QEMU_TIMEOUT_S = 60


def fail(msg: str, code: int = 2) -> "NoReturn":  # noqa: F821
    print(f"[qemu-test-runner] ERROR: {msg}", file=sys.stderr)
    dump_serial()
    sys.exit(code)


def dump_serial() -> None:
    """把串口日志打印到 stdout（cargo test 会展示），便于失败诊断。"""
    print("[qemu-test-runner] ===== serial output =====")
    try:
        with open(SERIAL_LOG, "r", encoding="utf-8", errors="replace") as f:
            print(f.read(), end="")
    except OSError:
        print("(no serial output captured)")
    print("[qemu-test-runner] ===== end serial =====")


def main() -> None:
    if len(sys.argv) < 2:
        fail("missing test binary path (argv[1])")
    elf = os.path.abspath(sys.argv[1])
    if not os.path.isfile(elf):
        fail(f"test binary not found: {elf}")

    qemu = shutil.which("qemu-system-x86_64")
    if qemu is None:
        fail("qemu-system-x86_64 not found in PATH")

    os.makedirs(os.path.dirname(IMG_PATH), exist_ok=True)
    os.makedirs(os.path.dirname(SERIAL_LOG), exist_ok=True)

    # 1. 打包磁盘镜像
    r = subprocess.run(
        [sys.executable, BUILD_DISK, elf, IMG_PATH],
        cwd=REPO_ROOT, capture_output=True, text=True,
    )
    if r.returncode != 0:
        fail(f"build_disk.py failed (exit {r.returncode}):\n{r.stdout}\n{r.stderr}")

    # 2. QEMU 执行
    cmd = [
        qemu,
        "-drive", f"file={IMG_PATH},format=raw",
        "-display", "none",
        "-serial", f"file:{SERIAL_LOG}",
        "-device", "isa-debug-exit,iobase=0x502",
        "-no-reboot", "-monitor", "none",
        "-m", "1024M", "-cpu", "qemu64",
    ]
    try:
        r = subprocess.run(cmd, cwd=REPO_ROOT, timeout=QEMU_TIMEOUT_S,
                           capture_output=True, text=True)
    except subprocess.TimeoutExpired:
        fail(f"QEMU timed out after {QEMU_TIMEOUT_S}s (test hung?)")

    # 3. 退出码翻译 + 输出
    dump_serial()
    if r.returncode == EXIT_ALL_PASS:
        print("[qemu-test-runner] PASS (QEMU exit 103)")
        sys.exit(0)
    elif r.returncode == EXIT_HAS_FAIL:
        print("[qemu-test-runner] FAIL (QEMU exit 175 — a test panicked)",
              file=sys.stderr)
        sys.exit(1)
    else:
        fail(f"unexpected QEMU exit code {r.returncode}\n"
             f"qemu stderr: {r.stderr.strip()[:500]}")


if __name__ == "__main__":
    main()
