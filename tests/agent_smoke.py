#!/usr/bin/env python3
"""xTB-Pilot agent 冒烟脚本（P4 验收形态，Windows 侧同构：裸 socket + NDJSON）。

流程：sys.health → mol.create（苯）→ job.submit（opt）→ 订阅 job.events
推送监控 → job.tail / res.scalar / res.export 回收。零 SDK、仅标准库。
"""

import json
import select
import socket
import sys
import time

HOST = "127.0.0.1"
PORT = 7700
TOKEN = sys.argv[1] if len(sys.argv) > 1 else "dev-token"


class Agent:
    """极简 NDJSON JSON-RPC 客户端（裸 socket）。"""

    def __init__(self, host, port, token):
        self.sock = socket.create_connection((host, port), timeout=10)
        self.sock.settimeout(None)  # 由 select 控制等待
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
                raise ConnectionError("连接被对端关闭")
            msg = json.loads(line)
            if "id" in msg and msg["id"] == req["id"]:
                if "error" in msg:
                    raise RuntimeError(f"RPC 错误: {msg['error']}")
                return msg.get("result")
            if "method" in msg:  # 服务器推送（订阅事件）
                self.events.append(msg)

    def subscribe(self, channel):
        return self.call(channel, {"subscribe": True})

    def wait_event(self, predicate, timeout=600):
        deadline = time.time() + timeout
        while time.time() < deadline:
            r, _, _ = select.select([self.sock], [], [], min(5, max(0, deadline - time.time())))
            if not r:
                continue
            line = self.f.readline()
            if not line:
                raise ConnectionError("连接被对端关闭")
            msg = json.loads(line)
            if "method" in msg and predicate(msg):
                return msg
        raise TimeoutError("等待事件超时")


def main():
    agent = Agent(HOST, PORT, TOKEN)
    agent.events = []

    # 1) 健康检查
    health = agent.call("sys.health")
    print(f"[1] sys.health ok={health['ok']} daemon={health['daemon']}")
    assert health["data_dir_on_linux_fs"], "/mnt 红线违规"

    # 2) 建分子（苯）
    mol = agent.call("mol.create", {"smiles": "c1ccccc1", "charge": 0, "multiplicity": 1})
    mol_id = mol["id"]
    print(f"[2] mol.create {mol['smiles']} → {mol_id} inchikey={mol['inchikey'][:14]}")

    # 3) 订阅 + 提交 opt
    agent.subscribe("job.events")
    job = agent.call("job.submit", {"molecule_id": mol_id, "workflow": "opt"})
    job_id = job["job_id"]
    print(f"[3] job.submit opt → {job_id} reused={job['reused']}")

    # 4) 事件推送监控（无轮询）；幂等复用则跳过（无新运行）
    if not job["reused"]:
        events = {"started": 0, "finished": None}
        while events["finished"] is None:
            ev = agent.wait_event(
                lambda m: m["method"] == "job.events" and m["params"].get("job_id") == job_id
            )
            ty = ev["params"]["type"]
            if ty == "started":
                events["started"] += 1
                print(f"[4] 事件: started")
            elif ty == "finished":
                events["finished"] = ev["params"]
                print(f"[4] 事件: finished ok={ev['params']['ok']}")

        assert events["finished"]["ok"], f"任务失败: {events['finished']}"
    else:
        print("[4] 幂等命中，跳过事件等待（无新计算）")

    # 5) 状态 / tail / 结果
    status = agent.call("job.status", {"job_id": job_id})
    print(f"[5] 状态: {status['status']} workdir={status.get('workdir', '-')}")
    tail = agent.call("job.tail", {"job_id": job_id, "offset": 0, "limit": 1000})
    lines = tail["lines"]
    n_energy = sum(1 for l in lines if "energy" in l["line"].lower())
    print(f"[5] tail 共 {len(lines)} 行，含 energy 行 {n_energy}")

    scalars = agent.call("res.scalar", {"job_id": job_id})
    energy = next(
        (s for s in scalars["scalars"] if s["key"] == "opt.total_energy"), None
    )
    assert energy, f"缺 total_energy: {scalars}"
    print(f"[5] 苯 GFN2-xTB 优化能量: {energy['value']:.6f} {energy['unit']} "
          f"(tier={energy['tier']})")
    # 文献/冒烟参考值：-15.8796 Eh（tests/chem/README 实测）
    assert abs(energy["value"] + 15.8796) < 0.01, "能量与实测偏差过大"

    # 6) 导出
    exported = agent.call("res.export", {"job_id": job_id, "format": "json"})
    print(f"[6] res.export → {exported['files']}")

    # 7) 幂等：重复提交命中缓存
    job2 = agent.call("job.submit", {"molecule_id": mol_id, "workflow": "opt"})
    assert job2["reused"], "重复提交应命中缓存"
    print(f"[7] 幂等命中缓存: {job2['job_id']}")

    # 8) dry-run 校验
    preview = agent.call(
        "job.submit",
        {"smiles": "CCO", "workflow": "conformer", "dry_run": True},
    )
    assert preview["dry_run"]
    print(f"[8] dry-run 预览: workflow={preview['workflow']}")

    print("\n✅ P4 冒烟闭环通过：提交 → 订阅监控 → 回收导出（裸 socket 无 SDK）")


if __name__ == "__main__":
    main()
