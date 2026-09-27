#!/usr/bin/env python3
"""qa_screendump_nostop.py — 不用 stop，每秒直接 screendump。
用于诊断 stop+cont 是否让 display surface 卡住。
"""
import json, os, socket, subprocess, sys, time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
LOGS = os.path.join(ROOT, "logs")
IMG = os.path.join(ROOT, "kernel_hd.img")
QMP_PORT = 4742


def qmp_cmd(sock, f, cmd, args=None):
    msg = {"execute": cmd}
    if args:
        msg["arguments"] = args
    sock.sendall((json.dumps(msg) + "\r\n").encode())
    while True:
        line = f.readline()
        if not line:
            raise RuntimeError("QMP closed")
        obj = json.loads(line)
        if "return" in obj or "error" in obj:
            return obj


def main():
    times = [float(x) for x in sys.argv[1:]] or [3.0, 5.0, 7.0, 9.0, 11.0]
    os.makedirs(LOGS, exist_ok=True)
    proc = subprocess.Popen(
        [
            "qemu-system-x86_64", "-drive", f"file={IMG},format=raw",
            "-display", "none", "-serial", f"file:{os.path.join(LOGS, 'qa_serial.log')}",
            "-device", "isa-debug-exit,iobase=0x502", "-no-reboot",
            "-m", "1024M", "-cpu", "qemu64",
            "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server,nowait",
        ],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        sock = None
        for _ in range(50):
            time.sleep(0.2)
            try:
                sock = socket.create_connection(("127.0.0.1", QMP_PORT), timeout=2)
                break
            except OSError:
                continue
        if sock is None:
            raise RuntimeError("QMP connect failed")
        f = sock.makefile("rb")
        greeting = json.loads(f.readline())
        qmp_cmd(sock, f, "qmp_capabilities")

        t0 = time.time()
        for t in times:
            wait = t - (time.time() - t0)
            if wait > 0:
                time.sleep(wait)
            ppm = os.path.join(LOGS, f"qan_{t:.1f}.ppm")
            r = qmp_cmd(sock, f, "screendump", {"filename": ppm.replace("\\", "/")})
            if "error" in r:
                print("screendump error:", r)
            png = os.path.join(LOGS, f"qan-{t:.1f}s.png")
            try:
                from PIL import Image
                Image.open(ppm).save(png)
                os.remove(ppm)
                print("captured", png)
            except Exception as e:
                print("ppm->png failed:", e)
        qmp_cmd(sock, f, "quit")
    finally:
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    main()