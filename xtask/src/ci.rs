//! CI 模式：`build` + 启动 QEMU + 轮询 `serial.log` + 断言启动成功。
//!
//! 退出条件（按先到者）：
//! * `serial.log` 中出现 `EXPECTED_SERIAL` → 成功
//! * QEMU 提前退出（status != 0）         → 失败（triple fault / panic）
//! * 达到 timeout                          → 失败（kernel 卡死或启动过慢）
//!
//! 无论成功失败，进程结束前都会 `kill` QEMU，避免 CI runner 残留。

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use crate::build;
use crate::qemu;

/// 启动成功的标志性字符串——`_start64` 直接往 COM1 (0x3F8) 写 `'H', 'i'`。
const EXPECTED_SERIAL: &str = "Hi";

/// 轮询间隔。200ms 足够捕获启动（通常 <1s），同时避免 CPU 空转。
const POLL_INTERVAL_MS: u64 = 200;

pub fn run(timeout_secs: u64) -> Result<(), String> {
    // Step 1: 构建
    build::run(false)?;

    // Step 2: 启动 QEMU
    let mut child = qemu::spawn_headless()?;

    // Step 3: 轮询 logs/serial.log
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask manifest has parent")
        .to_path_buf();
    let serial = root.join("logs").join("serial.log");
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut found = false;

    while Instant::now() < deadline {
        // 检查 QEMU 是否已退出
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let _ = child.kill();
                    return Err(format!("QEMU exited early (triple fault?): {status}"));
                }
                // QEMU 已干净退出（hlt 循环 + 外部 kill）——继续读 serial 一次再判断
                break;
            }
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                return Err(format!("QEMU wait error: {e}"));
            }
        }

        if let Ok(content) = std::fs::read_to_string(&serial) {
            if content.contains(EXPECTED_SERIAL) {
                found = true;
                break;
            }
        }

        thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
    }

    // Step 4: 兜底检查（QEMU 刚退出时 serial 可能刚写完）
    if !found {
        if let Ok(content) = std::fs::read_to_string(&serial) {
            if content.contains(EXPECTED_SERIAL) {
                found = true;
            }
        }
    }

    // Step 5: 终止 QEMU（如仍在跑）
    let _ = child.kill();
    let _ = child.wait();

    if found {
        println!("[xtask ci] OK — serial.log contains {EXPECTED_SERIAL:?}");
        Ok(())
    } else {
        let content = std::fs::read_to_string(&serial).unwrap_or_default();
        Err(format!(
            "[xtask ci] FAILED within {timeout_secs}s — serial.log content: {content:?}"
        ))
    }
}
