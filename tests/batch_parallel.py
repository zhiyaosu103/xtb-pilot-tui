#!/usr/bin/env python3
"""xTB-Pilot 批处理并行调度实弹演练（设计文档 §3.3 调度器验收形态）。

21 个 CSD 含硼分子 × 3 种工作流 = 63 个任务，一次全量提交，考验
并行计算与调度能力：

  1) 单点能 sp（GFN2-xTB --sp，优先级 0 交互优先）
  2) 激发态 excited（opt → xtb4stda → stda，优先级 1）
  3) 重组能 reorg-4pt（四点法 λ_h/λ_e，优先级 2）

全程：
  - 订阅 job.events / queue.events 记录每个任务的 started/finished
    与队列深度演化，统计峰值并发；
  - 真实 TUI（PTY）在线监控，周期抓取 Dashboard 的 q/r 统计；
  - 全部终态后汇总每分子 E_sp / E_vert / λ_h / λ_e 成表，
    输出调度统计（峰值并发、墙钟、优先级序、失败明细）。

隔离 daemon（临时数据目录/端口/UDS/并发槽），不污染正式环境。
"""

import json
import os
import random
import re
import select
import signal
import socket
import sys
import tempfile
import time

from tui_interaction import Tui, BareAgent, start_daemon, wait_port

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EH_TO_EV = 27.2114

# (SMILES, CSD refcode)
MOLECULES = [
    ("B1BN(c2ccccc2)C=CN1c1ccccc1", "csd_PAGREX"),
    ("B1Bc2ccsc2-c2sccc21", "csd_RUPRIE"),
    ("B1Bc2sccc2-c2ccsc21", "csd_TAYXIC"),
    ("B1C(c2ccccc2)=C(c2ccccc2)C(c2ccccc2)=C(c2ccccc2)N1N=C(c1ccccc1)c1ccccc1", "csd_FABMIH"),
    ("B1C(c2ccccc2)=C(c2ccccc2)C(c2ccccc2)=C(c2ccccc2)N1c1ccccc1", "csd_KEDTAR"),
    ("B1C(c2ccccc2)=C(c2ccccc2)C2=CN3BC(c4ccccc4)=C(c4ccccc4)C3=CN12", "csd_HEFQER"),
    ("B1C(c2ccccc2)=C(c2ccccc2)c2ccccc2-c2ccccc21", "csd_BIMKUG"),
    ("B1C(c2ccccc2)=C1c1ccccc1", "csd_CICGIG"),
    ("B1C2=C(c3ccccc3)[SiH2]C(c3ccccc3)=C2C2CCCC1CCC2", "csd_MUGXES"),
    ("B1C2=C(c3ccccc3CC2)c2cccc3cccc1c23", "csd_CEHLUZ"),
    ("B1C2=C(c3ccccc3NC2)c2cccc3cccc1c23", "csd_CEHNUB"),
    ("B1C=Cc2cc3ccccc3c(-c3ccc(N(c4ccccc4)c4ccccc4)cc3)c2N1", "csd_GUSYAW"),
    ("B1C=Cc2cc3ccccc3cc2N1", "csd_MAGSOF"),
    ("B1C=Pc2ccccc2-c2ccccc21", "csd_LIGMEW"),
    ("B1CC(c2ccccc2)=NN1c1ccccc1", "csd_COXROY"),
    ("B1CC2C(c3ccccc3)=C(c3ccccc3)C(c3ccccc3)=C2c2ccccc21", "csd_TACCUV"),
    ("B1Cc2ccccc2-c2ccccc21", "csd_COJGEQ"),
    ("B1N(c2ccccc2)C2=C(c3cccc4cccc2c34)N1c1ccccc1", "csd_BESQOH"),
    ("B1N(c2ccccc2)C=CN1c1ccccc1", "csd_GOQKOM"),
    ("B1N(c2ccccc2)C=NN1c1ccccc1", "csd_URACUN"),
    ("B1NC(c2ccccc2)=NO1", "csd_CUWHIN"),
]

WORKFLOWS = [("sp", 0), ("excited", 1), ("reorg-4pt", 2)]


class EventAgent:
    """订阅 job.events / queue.events 的事件流客户端。"""

    def __init__(self, port, token):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.sock.settimeout(None)
        self.f = self.sock.makefile("rwb")
        self.token = token
        self.next_id = 1
        self.events = []  # 未消费的推送

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
            self.events.append(msg)

    def drain(self, timeout=1.0):
        """收集直到空闲 timeout 秒，返回新事件。"""
        got = []
        while True:
            r, _, _ = select.select([self.sock], [], [], timeout)
            if not r:
                return got
            line = self.f.readline()
            if not line:
                raise ConnectionError("closed")
            msg = json.loads(line)
            self.events.append(msg)
            got.append(msg)


def main():
    tmp = tempfile.mkdtemp(prefix="xtbp-batch-")
    port = random.randint(20000, 40000)
    uds = os.path.join(tmp, "x.sock")
    token = "batch-token"
    slots = 4
    daemon = start_daemon(tmp, port, uds, token, max_concurrent=slots)
    run_id = time.strftime("%Y%m%d-%H%M%S")
    out_dir = os.path.join(REPO, "results", f"batch-{run_id}")
    os.makedirs(out_dir, exist_ok=True)
    t_start = time.time()
    try:
        assert wait_port(port), "隔离 daemon 启动失败"
        agent = EventAgent(port, token)
        agent.call("job.events", {"subscribe": True})
        agent.call("queue.events", {"subscribe": True})

        # ---- 1) 建 21 个分子（每个一次 rdkit gen3d 校验）----
        mol_ids = {}
        for smi, ref in MOLECULES:
            mol = agent.call("mol.create", {"smiles": smi, "charge": 0, "multiplicity": 1})
            mol_ids[ref] = mol["id"]
        print(f"[1] 21 个分子入库 ✓（{time.time()-t_start:.0f}s）")

        # ---- 2) 全量提交 63 个任务 ----
        # 每分子先提交 reorg（优先级 2）再 excited（1）再 sp（0）：
        # 提交最晚的 sp 因优先级 0 反而最先起跑——优先级调度效果可观测
        submitted = []  # (refcode, workflow, priority, job_id)
        for ref, mid in mol_ids.items():
            for wf, prio in reversed(WORKFLOWS):  # reorg → excited → sp
                job = agent.call("job.submit", {
                    "molecule_id": mid, "workflow": wf,
                    "priority": prio,
                    "params": {"extra": {"batch_tag": f"{run_id}-{ref}-{wf}"}},
                })
                submitted.append((ref, wf, prio, job["job_id"]))
        assert len({j[3] for j in submitted}) == 63, "63 个任务应全部唯一"
        print(f"[2] 63 个任务全量提交 ✓（{time.time()-t_start:.0f}s）")

        # ---- 3) TUI 在线监控（PTY 真实会话）----
        tui = Tui(["--uds", uds, "--no-spawn", "--token", token])
        tui.wait_for("已连接 daemon", timeout=30, desc="TUI 连接")
        tui_samples = []
        last_sample = time.time()

        # ---- 4) 事件循环：记录 started/finished、队列深度、峰值并发 ----
        meta = {j[3]: {"ref": j[0], "wf": j[1], "prio": j[2]} for j in submitted}
        started_at, finished_at, ok_map = {}, {}, {}
        running_seen = set()
        queue_history = []
        max_running = 0
        start_order = []  # 父任务 running 顺序（优先级验证）
        last_activity = time.time()

        def sample_tui(force=False):
            nonlocal last_sample
            now = time.time()
            if not force and now - last_sample < 10:
                return
            last_sample = now
            try:
                for line in tui.screen():
                    m = re.search(r"q(\d+)/r(\d+)", line)
                    if m:
                        tui_samples.append((round(now - t_start, 1), int(m.group(1)), int(m.group(2))))
                        return
            except Exception:
                pass

        while True:
            events = agent.drain(timeout=1.0)
            for ev in events:
                p = ev.get("params") or {}
                if ev.get("method") == "queue.events":
                    queue_history.append((time.time() - t_start, p.get("queued", 0), p.get("running", 0)))
                    max_running = max(max_running, p.get("running", 0))
                    continue
                if ev.get("method") != "job.events":
                    continue
                jid = p.get("job_id", "")
                ty = p.get("type", "")
                if jid not in meta:
                    continue  # 只统计 63 个父任务
                t = time.time() - t_start
                if ty == "status" and p.get("status") == "running" and jid not in started_at:
                    started_at[jid] = t
                    start_order.append(jid)
                    running_seen.add(jid)
                elif ty == "finished":
                    finished_at[jid] = t
                    ok_map[jid] = bool(p.get("ok"))
                    running_seen.discard(jid)
                last_activity = t
            sample_tui()
            if len(finished_at) >= 63:
                break
            if time.time() - t_start > 2400:
                # 兜底：轮询 job.list 终态（事件可能迟到）
                try:
                    jobs = agent.call("job.list", {"limit": 2000})
                    done = {j["id"] for j in jobs if j["status"] in ("done", "failed", "cancelled")}
                    for _, _, _, jid in submitted:
                        if jid not in finished_at and jid in done:
                            st = agent.call("job.status", {"job_id": jid})
                            finished_at[jid] = time.time() - t_start
                            ok_map[jid] = st["status"] == "done"
                    if len(finished_at) >= 63:
                        break
                except Exception:
                    pass
                if time.time() - t_start > 2700:
                    raise AssertionError("批处理超时（2700s），任务未全部终态")

        t_wall = time.time() - t_start
        print(f"[3] 全部 63 任务终态 ✓（墙钟 {t_wall:.0f}s）")

        # ---- 5) 调度统计 ----
        prio_running_order = [meta[j]["prio"] for j in start_order]
        mean_start = {}
        for p in (0, 1, 2):
            times = [started_at[j] for j in start_order if meta[j]["prio"] == p]
            mean_start[p] = sum(times) / len(times) if times else float("inf")
        last_start_prio = prio_running_order[-1] if prio_running_order else -1
        assert len(started_at) == 63, f"应有 63 个 started: {len(started_at)}"
        assert max_running >= 2, f"应观察到并发运行（峰值 {max_running}）"
        # 优先级：平均起跑时间 sp(0) < excited(1) < reorg(2)；最后起跑的应为 reorg
        prio_ok = (
            mean_start[0] < mean_start[1] < mean_start[2]
            and last_start_prio == 2
        )
        print(
            f"[4] 峰值并发运行: {max_running}（槽位 {slots}）| "
            f"平均起跑 sp {mean_start[0]:.0f}s < excited {mean_start[1]:.0f}s < reorg {mean_start[2]:.0f}s | "
            f"最后起跑优先级 {last_start_prio} → 优先级序 {'✓' if prio_ok else '✗'}"
        )

        # 各工作流耗时
        wf_times = {}
        for jid, t in finished_at.items():
            wf = meta[jid]["wf"]
            wf_times.setdefault(wf, []).append(t - started_at.get(jid, 0))

        # ---- 6) 汇总每分子结果 ----
        failed = []
        scalars_by_job = {}
        for ref, wf, prio, jid in submitted:
            st = agent.call("job.status", {"job_id": jid})
            scal = {}
            try:
                r = agent.call("res.scalar", {"job_id": jid})
                scal = {s["key"]: s["value"] for s in r["scalars"]}
            except Exception:
                pass
            scalars_by_job[(ref, wf)] = (st["status"], scal)
            if st["status"] != "done":
                failed.append((ref, wf, st["status"], st.get("error_code"), (st.get("error_message") or "")[:120]))

        header = ["refcode", "E_sp (Eh)", "E_vert (eV)", "λ_h (eV)", "λ_e (eV)", "备注"]
        table = []
        for ref, _ in MOLECULES:
            notes = []
            for wf in ("sp", "excited", "reorg"):
                st_wf, _ = scalars_by_job.get((ref, wf), ("missing", {}))
                if st_wf != "done":
                    notes.append(f"{wf}失败")
            st_sp, sc_sp = scalars_by_job.get((ref, "sp"), ("missing", {}))
            st_ex, sc_ex = scalars_by_job.get((ref, "excited"), ("missing", {}))
            st_rg, sc_rg = scalars_by_job.get((ref, "reorg"), ("missing", {}))
            e_sp = sc_sp.get("total_energy") if st_sp == "done" else None
            e_vert = (
                sc_ex.get("stda.first_excitation_energy")
                if st_ex == "done" else None
            ) or (sc_ex.get("first_excitation_energy") if st_ex == "done" else None)
            lh = sc_rg.get("lambda_h") if st_rg == "done" else None
            le = sc_rg.get("lambda_e") if st_rg == "done" else None
            table.append([
                ref,
                f"{e_sp:.6f}" if e_sp is not None else "-",
                f"{e_vert:.3f}" if e_vert is not None else "-",
                f"{lh * EH_TO_EV:.3f}" if lh is not None else "-",
                f"{le * EH_TO_EV:.3f}" if le is not None else "-",
                "；".join(notes),
            ])

        # ---- 7) 报告 ----
        lines = [f"# xTB-Pilot 批处理并行调度演练报告（{run_id}）", ""]
        lines += [
            "## 环境与负载",
            f"- 隔离 daemon 并发槽: {slots}（OMP 每任务 1 线程）",
            f"- 分子数: {len(MOLECULES)}（CSD 含硼化合物）× 工作流 sp/excited/reorg-4pt = {len(submitted)} 任务",
            f"- 墙钟总耗时: {t_wall:.0f}s；峰值并发运行: {max_running}；优先级序（前 {slots} 个开始任务）: {first_batch}",
            "",
            "## 每分子结果",
            "| " + " | ".join(header) + " |",
            "|" + "---|" * len(header),
        ]
        for row in table:
            lines.append("| " + " | ".join(str(x) for x in row) + " |")
        lines += ["", "## 调度统计"]
        lines += [f"- 各工作流平均耗时: " + "；".join(f"{wf} {sum(v)/len(v):.0f}s (n={len(v)})" for wf, v in sorted(wf_times.items()))]
        lines += [f"- 队列深度采样点数: {len(queue_history)}；TUI 观测采样点数: {len(tui_samples)}"]
        lines += ["", "## TUI 观测（部分采样，q=排队 r=运行）", "```"]
        lines += [f"  t={s[0]:>6.1f}s  q={s[1]:<3d} r={s[2]}" for s in tui_samples[:: max(1, len(tui_samples) // 12)]]
        lines += ["```"]
        if failed:
            lines += ["", "## 失败明细", "| refcode | workflow | status | error_code | error_message |", "|---|---|---|---|---|"]
            for ref, wf, st, code, msg in failed:
                lines.append(f"| {ref} | {wf} | {st} | {code} | {msg} |")
        lines += ["", f"失败任务数: {len(failed)} / {len(submitted)}"]

        report = "\n".join(lines)
        with open(os.path.join(out_dir, "summary.md"), "w") as f:
            f.write(report + "\n")
        # CSV 便于后续处理
        with open(os.path.join(out_dir, "per_molecule.csv"), "w") as f:
            f.write(",".join(header) + "\n")
            for row in table:
                f.write(",".join(str(x) for x in row) + "\n")
        print(report)

        # ---- 8) TUI 收尾：q 退出 ----
        tui.press("q", settle=1.0)
        code = tui.wait_exit()
        assert code == 0, f"TUI q 退出码应为 0: {code}"
        print(f"[5] TUI 全程在线监控（{len(tui_samples)} 次采样）并正常退出 ✓")
        print(f"\n报告: {out_dir}/summary.md")

        # 验收：全部终态 + 并行 + 优先级序
        assert len(finished_at) == 63 and max_running >= 2 and prio_ok
        print("\n✅ 批处理并行调度演练通过：63/63 终态、峰值并发 ≥2、优先级序正确")
    finally:
        daemon.terminate()
        daemon.wait(timeout=10)


if __name__ == "__main__":
    main()
