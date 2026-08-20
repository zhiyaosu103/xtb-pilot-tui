#!/usr/bin/env python3
"""daemon 空闲自毁测试（工程规范 §2.2）。

- 从未有过任务：不自毁（避免刚拉起就退出）；
- 提交任务并完成后：持续空闲 N 秒（--idle-shutdown-secs）→ 自动退出；
- 自毁是优雅停机：无残留 daemon 进程。
"""

import json
import os
import random
import socket
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from tui_interaction import BareAgent, start_daemon, wait_port

IDLE = 3  # 秒


def main():
    tmp = tempfile.mkdtemp(prefix="xtbp-idle-")
    port = random.randint(20000, 40000)
    uds = os.path.join(tmp, "x.sock")
    token = "idle-token"
    daemon = start_daemon(tmp, port, uds, token, max_concurrent=1, idle_shutdown_secs=IDLE)
    try:
        assert wait_port(port), "隔离 daemon 启动失败"
        agent = BareAgent(port, token)

        # 1) 从未有过任务：等待超过 IDLE 也不自毁
        time.sleep(IDLE + 2)
        assert daemon.poll() is None, "从未有过任务时 daemon 不应自毁"
        print(f"[1] 无任务空闲 {IDLE + 2}s → daemon 保持存活 ✓")

        # 2) 提交一个快速任务（sp），等待完成
        job = agent.call("job.submit", {"smiles": "CCO", "workflow": "sp"})
        jid = job["job_id"]
        deadline = time.time() + 120
        while time.time() < deadline:
            st = agent.call("job.status", {"job_id": jid})
            if st["status"] in ("done", "failed", "cancelled"):
                break
            time.sleep(0.5)
        assert st["status"] == "done", f"任务未完成: {st}"
        print(f"[2] 任务完成（{st['status']}）✓")

        # 3) 最后一个任务完成后：空闲 IDLE 秒 → daemon 自动退出
        deadline = time.time() + IDLE + 15
        while time.time() < deadline and daemon.poll() is None:
            time.sleep(0.5)
        assert daemon.poll() is not None, "任务完成后 daemon 应在空闲超时内自毁"
        print(f"[3] 空闲 {IDLE}s 后 daemon 自动退出 ✓")

        # 4) 无残留 xtbp 进程
        time.sleep(1)
        left = [
            l for l in subprocess.run(["ps", "-eo", "cmd"], capture_output=True, text=True).stdout.splitlines()
            if "xtbp-daemon" in l and "grep" not in l
        ]
        assert not left, f"应无残留 daemon: {left}"
        print("[4] 无残留进程 ✓")

        print("\n✅ 空闲自毁测试通过：无任务不自毁、任务完成后自动退出、无残留")
    finally:
        if daemon.poll() is None:
            daemon.terminate()
            daemon.wait(timeout=10)


if __name__ == "__main__":
    main()
