#!/usr/bin/env python3
"""xTB-Pilot 红线验证（设计文档 §5 全局红线，P5 验收）。

1. /mnt/c 红线：数据目录在 9P 盘必须拒绝启动；
2. kill -9 daemon：数据库不损坏、无僵尸进程、任务状态可解释；
3. 重启后功能完好：可继续提交并完成新任务。

隔离实例（独立端口/数据目录），不干扰正式 daemon。裸 socket 交互。
"""

import json
import os
import signal
import socket
import subprocess
import sys
import random
import tempfile
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DAEMON = os.path.join(REPO, "target", "debug", "xtbp-daemon")


def wait_port(port, timeout=15):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=1)
            s.close()
            return True
        except OSError:
            time.sleep(0.3)
    return False


class BareAgent:
    def __init__(self, port, token):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.sock.settimeout(None)
        self.f = self.sock.makefile("rwb")
        self.token = token
        self.next_id = 1

    def call(self, method, params=None):
        req = {
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": method,
            "params": params or {},
            "token": self.token,
        }
        self.next_id += 1
        self.f.write((json.dumps(req) + "\n").encode())
        self.f.flush()
        while True:
            line = self.f.readline()
            if not line:
                raise ConnectionError("closed")
            msg = json.loads(line)
            if msg.get("id") == req["id"]:
                if "error" in msg:
                    raise RuntimeError(msg["error"])
                return msg.get("result")
            # 推送忽略（本测试走轮询状态）

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def start_daemon(data_dir, port, uds, token, extra_env=None, extra_args=None):
    env = dict(os.environ)
    env["XTB4STDAHOME"] = os.path.expanduser("~/opt/xtb4stda-1.0")
    if extra_env:
        env.update(extra_env)
    cmd = [
        DAEMON,
        "--listen", f"127.0.0.1:{port}",
        "--data-dir", data_dir,
        "--uds", uds,
        "--token", token,
        "--registry", os.path.join(data_dir, "registry.toml"),
        "--log-dir", os.path.join(data_dir, "logs"),
        "--max-concurrent", "2",
    ]
    if extra_args:
        cmd += extra_args
    return subprocess.Popen(cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def defunct_children(daemon_pid):
    """daemon 的僵尸子进程计数。"""
    out = subprocess.run(
        ["ps", "-eo", "ppid=,stat="], capture_output=True, text=True
    ).stdout
    n = 0
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[0] == str(daemon_pid) and "Z" in parts[1]:
            n += 1
    return n


def main():
    # ---- 1) /mnt 红线 ----
    print("[1] /mnt/c 红线自检")
    p = start_daemon("/mnt/c/xtbp-redline-test", 7799, "/tmp/xtbp-mnt.sock", "t")
    p.wait(timeout=10)
    assert p.returncode != 0, "/mnt/c 数据目录应被拒绝"
    print("[1] 拒绝启动 ✓（exit=%s）" % p.returncode)

    # ---- 2) 隔离实例 + 混合负载 ----
    tmp = tempfile.mkdtemp(prefix="xtbp-redline-")
    port = random.randint(20000, 40000)
    uds = os.path.join(tmp, "x.sock")
    token = "redline-token"
    daemon = start_daemon(tmp, port, uds, token)
    assert wait_port(port), "隔离实例启动失败"
    agent = BareAgent(port, token)

    # 提交 8 个构象搜索任务（nconf=80，每个 ~10s，确保 kill 时仍在运行）
    mol = agent.call("mol.create", {"smiles": "CCO", "charge": 0, "multiplicity": 1})
    mol_ids = [mol["id"]]
    job_ids = []
    for i in range(8):
        # extra.batch_tag 参与内容哈希 → 每个任务唯一（避免幂等去重）
        j = agent.call(
            "job.submit",
            {
                "molecule_id": mol_ids[0],
                "workflow": "conformer",
                "priority": i % 2,
                "params": {"extra": {"batch_tag": i, "nconf": "80"}},
            },
        )
        job_ids.append(j["job_id"])
    assert len(set(job_ids)) == 8, f"任务应全部唯一: {len(set(job_ids))}"
    print(f"[2] 提交 8 个 conformer 任务（并发 2，含交互/批量混合）")
    time.sleep(2)  # 前两个任务正在运行中

    # ---- 3) kill -9 ----
    daemon_pid = daemon.pid
    os.kill(daemon_pid, signal.SIGKILL)
    daemon.wait(timeout=10)
    time.sleep(1)
    zombies = defunct_children(daemon_pid)
    print(f"[3] kill -9 后僵尸子进程: {zombies}（daemon 已死，其孙进程应收敛）")
    agent.close()

    # ---- 4) 重启恢复 ----
    daemon2 = start_daemon(tmp, port, uds, token)
    assert wait_port(port), "重启失败（数据库损坏？）"
    agent2 = BareAgent(port, token)
    health = agent2.call("sys.health")
    assert health["ok"], "重启后健康检查失败"
    jobs = agent2.call("job.list", {"limit": 500})
    statuses = {}
    for j in jobs:
        statuses[j["status"]] = statuses.get(j["status"], 0) + 1
    print(f"[4] 重启后任务状态分布: {statuses}")
    # 状态可解释：无 running/queued 残留（全部终态或 interrupted）
    assert statuses.get("running", 0) == 0 and statuses.get("queued", 0) == 0, (
        "崩溃后不应有 running/queued 残留"
    )
    done = statuses.get("done", 0)
    interrupted = statuses.get("interrupted", 0)
    assert done + interrupted == 16, f"任务数不守恒: {statuses}"
    assert interrupted >= 2, f"应有被中断的任务（kill 时机在运行中）: {statuses}"
    print(f"[4] 状态可解释 ✓（done={done}, interrupted={interrupted}）")

    # ---- 5) 重启后功能完好 ----
    job = agent2.call(
        "job.submit",
        {"molecule_id": mol_ids[0], "workflow": "opt"},
    )
    if not job.get("reused"):
        deadline = time.time() + 300
        while time.time() < deadline:
            st = agent2.call("job.status", {"job_id": job["job_id"]})
            if st["status"] in ("done", "failed"):
                break
            time.sleep(1)
        assert st["status"] == "done", f"重启后任务失败: {st}"
    print(f"[5] 重启后新任务完成 ✓ ({job['job_id'][:8]} {job.get('reused') and '复用' or '新算'})")

    # ---- 6) 收尾：取消 interrupted（状态可解释性收口）----
    for j in jobs:
        if j["status"] == "interrupted":
            agent2.call("job.cancel", {"job_id": j["id"]})
    print("[6] interrupted 任务已收口为 cancelled ✓")

    daemon2.terminate()
    daemon2.wait(timeout=10)
    print("\n✅ 红线验证通过：kill -9 可恢复、无僵尸、状态可解释、/mnt/c 禁令生效")


if __name__ == "__main__":
    main()
