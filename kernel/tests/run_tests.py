#!/usr/bin/env python3
"""P1-T8 内核测试一站式入口：build → package → QEMU → 退出码。

用法（仓库根目录）::

    python kernel/tests/run_tests.py            # 构建并运行 basic_boot
    python kernel/tests/run_tests.py --no-build # 跳过构建，直接运行最新产物

## 为什么不是裸 `cargo test`

nightly-2026-09-23 在 Windows 上 `cargo test` + `-Zbuild-std` 会构建
**两份 core**（`build/core/798f28ba35f0ee40`（`\\\\?\\` UNC 路径指纹）与
`build/core/3dbc56a12d49c31f`（普通路径指纹）），依赖 crate 编译时同时
加载两份 → `E0152 duplicate lang item in crate core: sized`。
已验证与 cwd、profile（--profile dev）、runner 环境变量无关；
`cargo build --test basic_boot` 单一 core、构建稳定，故测试管线为：

    cargo build --test  →  qemu_test_runner.py（打包 + QEMU + 退出码翻译）

退出码：0 = 全部通过；1 = 存在失败；2 = 构建/打包/QEMU 异常。
"""

import glob
import os
import subprocess
import sys

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))   # .../kernel/tests
REPO_ROOT = os.path.dirname(os.path.dirname(SCRIPT_DIR))  # .../ai-os
RUNNER = os.path.join(SCRIPT_DIR, "qemu_test_runner.py")

CARGO_CMD = [
    "cargo", "build", "-p", "synapse-kernel", "--test", "basic_boot",
    "--target", "x86_64-bootloader.json",
    "-Zjson-target-spec",
    "-Zbuild-std=core,alloc,compiler_builtins",
]

# cargo 对 JSON target 的测试产物位置（无 deps/ 目录，落在 build/<pkg>/<hash>/out/）
ELF_GLOB = os.path.join(
    REPO_ROOT, "target", "x86_64-bootloader", "debug",
    "build", "synapse-kernel", "*", "out", "basic_boot-*",
)


def find_elf() -> str:
    candidates = [p for p in glob.glob(ELF_GLOB) if not p.endswith(".d")]
    if not candidates:
        print(f"[run_tests] ERROR: no basic_boot ELF found ({ELF_GLOB})",
              file=sys.stderr)
        sys.exit(2)
    return max(candidates, key=os.path.getmtime)


def main() -> None:
    if "--no-build" not in sys.argv:
        print("[run_tests] building basic_boot ...")
        r = subprocess.run(CARGO_CMD, cwd=REPO_ROOT)
        if r.returncode != 0:
            print(f"[run_tests] ERROR: cargo build failed (exit {r.returncode})",
                  file=sys.stderr)
            sys.exit(2)

    elf = find_elf()
    print(f"[run_tests] running {os.path.relpath(elf, REPO_ROOT)} in QEMU ...")
    sys.exit(subprocess.run([sys.executable, RUNNER, elf],
                            cwd=REPO_ROOT).returncode)


if __name__ == "__main__":
    main()
