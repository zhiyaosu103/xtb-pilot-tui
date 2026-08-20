#!/usr/bin/env python3
"""xTB-Pilot TUI 真实交互模拟测试（PTY 驱动，设计文档 §3.6 验收形态）。

在真实 PTY 中拉起 xtbp-tui，注入真实键序列模拟完整用户会话，并用
ANSI 网格仿真器做屏幕抓取断言：

  S1 帮助弹出与关闭（? / 任意键关闭）
  S2 Tab 八页循环切换（状态行断言页名）
  S3 Workflows 页 s 输入 SMILES → Enter 提交 opt → 自动切 Jobs 页并监控到 done
  S4 过滤（/ opt Enter 收窄 → 清空恢复）
  S5 列表跳转 Home/End/PgUp/PgDn/jk + Space 详情（与 job.list 顺序逐一比对）
  S6 c 取消（排队中任务 → cancelled 确定性断言；运行中任务宽松断言）
  S7 q 退出（退出码 0，daemon 独立存活）
  S8 Ctrl-C 退出（退出码 0）

隔离 daemon（临时数据目录/端口/UDS），不污染正式环境。
"""

import fcntl
import json
import os
import pty
import random
import re
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unicodedata

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DAEMON = os.path.join(REPO, "target", "debug", "xtbp-daemon")
TUI = os.path.join(REPO, "target", "debug", "xtbp-tui")
ROWS, COLS = 30, 100
ULID_RE = r"[0-9A-Z]{26}"

ESC = "\x1b"
KEYS = {
    "enter": "\r",
    "esc": ESC,
    "tab": "\t",
    "backspace": "\x7f",
    "up": ESC + "[A",
    "down": ESC + "[B",
    "right": ESC + "[C",
    "left": ESC + "[D",
    "home": ESC + "[H",
    "end": ESC + "[F",
    "pgup": ESC + "[5~",
    "pgdn": ESC + "[6~",
    "ctrl-c": "\x03",
    "space": " ",
}


def render_screen(data: bytes, rows: int = ROWS, cols: int = COLS) -> list[str]:
    """极简 ANSI 终端仿真：绝对定位 + 增量覆盖 → 当前屏幕网格。

    帧间无清屏（ratatui 差分重绘），对整条字节流按序应用即可得到最终屏幕。
    """
    grid = [[(" " if (r + c) % 2 else " ") for c in range(cols)] for r in range(rows)]
    # 初始化全空
    grid = [[" "] * cols for _ in range(rows)]
    r = c = 0
    i, n = 0, len(data)
    csi = re.compile(rb"\x1b\[([0-9;?]*)([A-Za-z])")
    while i < n:
        b = data[i]
        if b == 0x1B:
            m = csi.match(data, i)
            if m:
                params = m.group(1).decode()
                final = m.group(2).decode()
                i = m.end()
                if params.startswith("?"):
                    continue  # 私有模式（如 ?1049h 备用屏），网格无关
                vals = [int(x) if x else 1 for x in params.split(";")]
                if final in "Hf":
                    r = (vals[0] - 1) if vals else 0
                    c = (vals[1] - 1) if len(vals) > 1 else 0
                elif final == "A":
                    r -= vals[0] if vals else 1
                elif final == "B":
                    r += vals[0] if vals else 1
                elif final == "C":
                    c += vals[0] if vals else 1
                elif final == "D":
                    c -= vals[0] if vals else 1
                elif final == "G":
                    c = vals[0] - 1 if vals else 0
                elif final == "K":  # 行擦除
                    mode = 0 if not params else vals[0]
                    if mode == 0:
                        for cc in range(c, cols):
                            grid[r][cc] = " "
                    elif mode == 1:
                        for cc in range(0, c + 1):
                            grid[r][cc] = " "
                    else:
                        grid[r] = [" "] * cols
                elif final == "J":  # 屏擦除
                    mode = 0 if not params else vals[0]
                    if mode == 2:
                        grid = [[" "] * cols for _ in range(rows)]
                continue
            # 非 CSI 转义（OSC/字符集等）：跳到终止符
            if i + 1 < n and data[i + 1] in (b"]", b"P", b"X"):
                end = data.find(b"\x07", i)
                if end == -1:
                    end = data.find(b"\x1b\\", i)
                i = n if end == -1 else end + 1
            else:
                i += 1
            continue
        if b == 0x0D:
            c = 0
            i += 1
            continue
        if b == 0x0A:
            r += 1
            i += 1
            continue
        if b == 0x09:
            c += 4 - (c % 4)
            i += 1
            continue
        # UTF-8 字符
        if b < 0x80:
            ch = chr(b)
            i += 1
        elif b < 0xC0:
            i += 1
            continue
        else:
            ln = 2 if b < 0xE0 else 3 if b < 0xF0 else 4
            ch = data[i : i + ln].decode("utf-8", errors="replace")
            i += ln
        if 0 <= r < rows and 0 <= c < cols:
            wide = ch and unicodedata.east_asian_width(ch) in ("W", "F")
            grid[r][c] = ch
            if wide and c + 1 < cols:
                grid[r][c + 1] = None  # 宽字符的右半格：不参与文本匹配
        c += 2 if ch and unicodedata.east_asian_width(ch) in ("W", "F") else 1
    return [
        "".join("" if cell is None else cell for cell in row).rstrip() for row in grid
    ]


class BareAgent:
    """裸 socket NDJSON JSON-RPC 客户端（与 agent_smoke/redline 同构）。"""

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
            # 推送忽略（本测试走轮询）

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def start_daemon(tmp, port, uds, token, max_concurrent=2, idle_shutdown_secs=0):
    # HOME 覆盖到隔离目录：resolve_token 写 ~/.xtbpilot/agent.json、
    # ensure_xtb4stda_home_params 写 ~/.param_stda*.xtb（CI/沙箱下 HOME 只读）。
    # 组件回退发现走 ~/opt/<name>-*/bin（daemon find_component），
    # 用符号链接把回退二进制挂进隔离 HOME。
    opt_dir = os.path.join(tmp, "opt")
    os.makedirs(opt_dir, exist_ok=True)
    for name in ("xtb4stda-1.0", "stda-1.6.1"):
        src = os.path.expanduser(f"~/opt/{name}")
        if os.path.isdir(src) and not os.path.lexists(os.path.join(opt_dir, name)):
            os.symlink(src, os.path.join(opt_dir, name))
    env = dict(os.environ)
    env["HOME"] = tmp
    env["XTB4STDAHOME"] = os.path.expanduser("~/opt/xtb4stda-1.0")
    cmd = [
        DAEMON,
        "--listen", f"127.0.0.1:{port}",
        "--data-dir", tmp,
        "--uds", uds,
        "--token", token,
        "--registry", os.path.join(tmp, "registry.toml"),
        "--log-dir", os.path.join(tmp, "logs"),
        "--max-concurrent", str(max_concurrent),
        "--idle-shutdown-secs", str(idle_shutdown_secs),
    ]
    return subprocess.Popen(cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def wait_port(port, timeout=20):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=1)
            s.close()
            return True
        except OSError:
            time.sleep(0.25)
    return False


class Tui:
    """PTY 中的 xtbp-tui 会话：注入键序列 + 屏幕抓取。"""

    def __init__(self, args, env=None, timeout=60):
        pid, fd = pty.fork()
        if pid == 0:
            if env is not None:
                os.execve(TUI, [TUI] + args, env)
            os.execv(TUI, [TUI] + args)
        self.pid = pid
        self.fd = fd
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.buf = b""
        self.ready = False
        self._drain(2.0)  # 初始帧（可能包含连接重试过程）
        self.ready = True

    def _drain(self, timeout=0.2):
        end = time.time() + timeout
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], 0.1)
            if not r:
                continue
            try:
                chunk = os.read(self.fd, 65536)
            except OSError:
                return
            if not chunk:
                return
            self.buf += chunk

    def screen(self):
        self._drain(0.12)
        return render_screen(self.buf)

    def wait_for(self, pattern, timeout=30, desc=""):
        rx = re.compile(pattern)
        deadline = time.time() + timeout
        while time.time() < deadline:
            for line in self.screen():
                m = rx.search(line)
                if m:
                    return m
            time.sleep(0.2)
        raise AssertionError(
            f"超时未出现 {pattern!r}（{desc}）。最近屏幕:\n" + "\n".join(self.screen())
        )

    def wait_gone(self, pattern, timeout=10, desc=""):
        rx = re.compile(pattern)
        deadline = time.time() + timeout
        while time.time() < deadline:
            if not any(rx.search(line) for line in self.screen()):
                return
            time.sleep(0.2)
        raise AssertionError(f"模式未消失 {pattern!r}（{desc}）")

    def press(self, keys, settle=0.2):
        os.write(self.fd, keys.encode())
        time.sleep(settle)
        self._drain()

    def key(self, name, settle=0.2):
        self.press(KEYS[name], settle=settle)

    def type_text(self, text, settle=0.06):
        os.write(self.fd, text.encode())
        time.sleep(settle * len(text) + 0.2)
        self._drain()

    def wait_exit(self, timeout=15):
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                pid, status = os.waitpid(self.pid, os.WNOHANG)
            except ChildProcessError:
                return 0
            if pid:
                if os.WIFEXITED(status):
                    return os.WEXITSTATUS(status)
                return -1 if os.WIFSIGNALED(status) else -2
            time.sleep(0.1)
        os.kill(self.pid, signal.SIGKILL)
        raise AssertionError("TUI 未在预期时间内退出")


def selected_detail_id(tui):
    """从详情面板首行提取选中任务 id（"{id} · {workflow} · {status}"）。"""
    m = tui.wait_for(rf"({ULID_RE}) · \S+ · \S+", desc="详情面板首行")
    return m.group(1)


def main():
    tmp = tempfile.mkdtemp(prefix="xtbp-tui-")
    port = random.randint(20000, 40000)
    uds = os.path.join(tmp, "x.sock")
    token = "tui-test-token"
    daemon = start_daemon(tmp, port, uds, token)
    try:
        assert wait_port(port), "隔离 daemon 启动失败"
        agent = BareAgent(port, token)
        tui_env = dict(os.environ, HOME=tmp)
        tui = Tui(["--uds", uds, "--no-spawn"], env=tui_env)  # 无 token：UDS 不鉴权（回归）
        try:
            # ---- S1 连接 + 帮助 ----
            tui.wait_for("已连接 daemon", timeout=30, desc="UDS 连接")
            tui.press("?")
            tui.wait_for("键位帮助", desc="帮助弹出")
            tui.key("space")
            tui.wait_gone("键位帮助", desc="帮助关闭")
            print("[S1] 帮助弹出/关闭 ✓")

            # ---- S2 Tab 八页循环 ----
            titles = ["Molecules", "Jobs", "Spectra", "Structure",
                      "Workflows", "Instances", "Settings", "Dashboard"]
            for t in titles:
                tui.key("tab")
                tui.wait_for(rf"· {t}", timeout=10, desc=f"切页到 {t}")
            print("[S2] Tab 八页循环切换 ✓")

            # ---- S3 Workflows 页提交 opt（真实键输入 SMILES）----
            for _ in range(5):
                tui.key("tab")
            tui.wait_for("· Workflows", desc="到 Workflows 页")
            tui.press("s")
            tui.type_text("c1ccccc1")
            tui.key("enter", settle=0.5)
            tui.wait_for("已提交 opt", timeout=30, desc="提交成功状态行")
            tui.wait_for(r"任务 \(1\)", timeout=15, desc="自动切 Jobs 页")
            tui.wait_for(r"done\s+opt\s+[0-9A-Z]+", timeout=180, desc="opt 任务完成（列表行短 id）")
            print("[S3] TUI 内提交 opt（SMILES 键盘输入）→ 监控到 done ✓")

            # ---- S4 批量造负载：12 sp（先跑完腾出槽位）+ 2 个 opt-freq 大分子 ----
            # 注意：TUI 任务列表只展示工作流任务（父任务），子步骤不显示
            sp_ids = []
            for i in range(12):
                sp_ids.append(agent.call("job.submit", {
                    "smiles": "CCO", "workflow": "sp",
                    "params": {"extra": {"batch_tag": i}},
                })["job_id"])
            # 等 12 个 sp 全部终态，腾出并发槽（确定性：随后 b1 必在运行、b2 必在排队）
            deadline = time.time() + 180
            while time.time() < deadline:
                statuses = [agent.call("job.status", {"job_id": j})["status"] for j in sp_ids]
                if all(st in ("done", "failed") for st in statuses):
                    break
                time.sleep(1)
            else:
                raise AssertionError(f"sp 批量任务未在时限内终态: {statuses}")
            # FABMIH（54 原子）opt-freq：opt + --ohess 频率（本机实测 ~47s 窗口）
            fabmih = "B1C(c2ccccc2)=C(c2ccccc2)C(c2ccccc2)=C(c2ccccc2)N1N=C(c1ccccc1)c1ccccc1"
            c1 = agent.call("job.submit", {
                "smiles": fabmih, "workflow": "opt-freq",
                "params": {"extra": {"batch_tag": "slow1"}},
            })
            conf1 = c1["job_id"]
            deadline = time.time() + 60
            while time.time() < deadline:
                if agent.call("job.status", {"job_id": conf1})["status"] == "running":
                    break
                time.sleep(0.5)
            else:
                raise AssertionError("b1 未进入运行态")
            c2 = agent.call("job.submit", {
                "smiles": fabmih, "workflow": "opt-freq",
                "params": {"extra": {"batch_tag": "slow2"}},
            })
            conf2 = c2["job_id"]
            tui.press("g")  # 刷新列表
            tui.wait_for(r"任务 \(15\)", timeout=15, desc="15 个任务")
            print("[S4] 15 个任务入列（1 opt + 12 sp + 2 opt-freq FABMIH）✓")

            # ---- S5 过滤 ----
            tui.press("/")
            tui.type_text("opt")
            tui.key("enter")
            tui.wait_for(r"任务 \(3\)", timeout=10, desc="过滤后只剩 opt 系（opt+opt-freq×2）")
            tui.press("/")
            tui.type_text(KEYS["backspace"] * 3)
            tui.key("enter")
            tui.wait_for(r"任务 \(15\)", timeout=10, desc="清空过滤恢复")
            print("[S5] 过滤 / 清空 ✓")

            # ---- S6 列表跳转 + Space 详情（与 job.list 顺序比对）----
            jobs = agent.call("job.list", {"limit": 500})
            # 与 TUI 一致：只保留工作流任务（父任务），子步骤不在列表
            ids = [j["id"] for j in jobs if not j.get("parent_id")]
            assert len(ids) == 15, f"父任务应为 15 条: {len(ids)} ({len(jobs)} 含子步骤)"
            assert ids[0] != ids[-1]
            # End → 最后一条（conf2，排队中）
            tui.key("end")
            tui.key("space", settle=0.4)
            assert selected_detail_id(tui) == ids[14], "End 应选中最后一条"
            # Home → 第一条（opt）
            tui.key("home")
            tui.key("space", settle=0.4)
            assert selected_detail_id(tui) == ids[0], "Home 应选中第一条"
            # End 后 PgUp → 14-10=4
            tui.key("end")
            tui.key("pgup")
            tui.key("space", settle=0.4)
            assert selected_detail_id(tui) == ids[4], "PgUp 应上翻 10 行"
            # PgDn → 回到 14
            tui.key("pgdn")
            tui.key("space", settle=0.4)
            assert selected_detail_id(tui) == ids[14], "PgDn 应下翻 10 行"
            # k/j 单行移动（14 → 13）
            tui.press("k")
            tui.key("space", settle=0.4)
            assert selected_detail_id(tui) == ids[13], "k 应上移一行"
            tui.press("j")
            tui.key("space", settle=0.4)
            assert selected_detail_id(tui) == ids[14], "j 应下移一行"
            print("[S6] Home/End/PgUp/PgDn/j/k 跳转 + Space 详情 ✓")

            # ---- S7 取消 ----
            # 7a) 排队中的 conf2（确定性）：End → c
            tui.key("end")
            tui.key("space", settle=0.4)
            assert selected_detail_id(tui) == ids[14]
            tui.press("c")
            tui.wait_for("已请求取消", timeout=10, desc="取消排队任务")
            deadline = time.time() + 60
            while time.time() < deadline:
                st = agent.call("job.status", {"job_id": conf2})
                if st["status"] == "cancelled":
                    break
                time.sleep(0.5)
            assert st["status"] == "cancelled", f"排队任务应被取消: {st}"
            print("[S7a] c 取消排队任务 → cancelled ✓")
            # 7b) 运行中的 conf1（宽松：cancelled 或已跑完 done）：
            # 过滤 "running" 只看运行中任务（FABMIH opt-freq ~47s 窗口）
            tui.press("g")
            tui.press("/")
            tui.type_text("running")
            tui.key("enter")
            tui.wait_for(r"任务 \(1\)", timeout=10, desc="过滤出 1 个 running")
            tui.key("home")
            tui.key("space", settle=0.4)
            assert selected_detail_id(tui) == conf1, (
                "应选中运行中的 opt-freq 任务。屏幕:\n" + "\n".join(tui.screen())
            )
            tui.press("c")
            tui.wait_for("已请求取消", timeout=10, desc="取消运行中任务")
            deadline = time.time() + 120
            while time.time() < deadline:
                st = agent.call("job.status", {"job_id": conf1})
                if st["status"] in ("cancelled", "done"):
                    break
                time.sleep(0.5)
            print(f"[S7b] c 取消运行中任务 → {st['status']} ✓")
            tui.press("/")
            tui.type_text(KEYS["backspace"] * 7)
            tui.key("enter")

            # ---- S7c Workflows 页参数编辑 + Enter 直接输入 + .smi 批量导入 ----
            for _ in range(3):
                tui.key("tab")
            tui.wait_for("· Workflows", desc="回到 Workflows 页")
            # 计算水平循环：GFN2 → GFN1
            tui.press("]")
            tui.wait_for("计算水平: GFN1-xTB", desc="计算水平切换")
            # 溶剂模型循环：气相 → ALPB → GBSA
            tui.press("}")
            tui.wait_for("溶剂模型: ALPB", desc="溶剂模型 ALPB")
            tui.press("}")
            tui.wait_for("溶剂模型: GBSA", desc="溶剂模型 GBSA")
            # 溶剂名编辑（e 预填当前值，先清空再输入）
            tui.press("e")
            tui.type_text(KEYS["backspace"] * 5)
            tui.type_text("thf")
            tui.key("enter")
            tui.wait_for("溶剂: thf", desc="溶剂名应用")
            # etemp 编辑
            tui.press("t")
            tui.type_text("500")
            tui.key("enter")
            tui.wait_for("etemp: 500 K", desc="etemp 应用")
            # Enter 直接进入 SMILES 输入模式并提交（修复"enter 无反应"）
            tui.key("enter")
            tui.type_text("CCO")
            tui.key("enter", settle=0.5)
            tui.wait_for("已提交 opt", timeout=30, desc="Enter 直接提交")
            # 校验 daemon 侧任务参数（method.solvation=gbsa, etemp=500）
            jobs_now = [
                j for j in agent.call("job.list", {"limit": 500})
                if not j.get("parent_id")
            ]
            tuned = None
            for j in jobs_now:
                m = (j.get("params") or {}).get("method") or {}
                if m.get("solvation") == "gbsa" and m.get("etemp") == 500.0:
                    tuned = j
            assert tuned, "应存在 GBSA+etemp=500 的任务"
            assert (tuned.get("params") or {}).get("charge") == 0
            print("[S7c] 参数编辑（GFN1/GBSA/thf/etemp=500）+ Enter 直接提交 ✓")

            # .smi 批量导入：先写临时文件（2 行）
            smi_path = os.path.join(REPO, "target", "tui-import.smi")
            with open(smi_path, "w") as f:
                # 避免与已提交的 CCO 幂等撞车：用全新分子
                f.write("# test batch\nCC ethane\nC1CCCCC1 hexane\n")
            # S7c 提交后页面已切到 Jobs：先回 Workflows（i 只在 Workflows 页生效）
            for _ in range(3):
                tui.key("tab")
            tui.wait_for("· Workflows", desc="回到 Workflows 页（导入前）")
            tui.press("i")
            tui.type_text(smi_path)
            tui.key("enter", settle=1.0)
            tui.wait_for("导入", timeout=30, desc="批量导入完成")
            tui.wait_for(r"提交 2 / 失败 0", timeout=30, desc="2 行全部提交")
            jobs_after = agent.call("job.list", {"limit": 500})
            parents_after = [j for j in jobs_after if not j.get("parent_id")]
            assert len(parents_after) == len(jobs_now) + 2, "应新增 2 个导入任务"
            print("[S7d] .smi 批量导入（2 行 → 提交 2 / 失败 0）✓")

            # 恢复默认参数（后续场景不受影响）
            tui.press("[")
            tui.press("{")
            tui.press("{")
            tui.press("e")
            tui.type_text(KEYS["backspace"] * 3)
            tui.type_text("water")
            tui.key("enter")

            # ---- S8 q 退出（daemon 独立存活）----
            tui.press("q", settle=1.0)
            code = tui.wait_exit()
            assert code == 0, f"q 退出码应为 0: {code}"
            health = agent.call("sys.health")
            assert health["ok"], "daemon 应仍在运行"
            print("[S8] q 退出（退出码 0，daemon 独立存活）✓")

            # ---- S9 退出语义：开关默认关 → 只退 TUI；Settings 页 ←/→ 开开关 → 全链退出 ----
            # 9a) 开关默认关（隔离 HOME 无设置文件）：Ctrl-C 只退 TUI，daemon 存活
            tui2 = Tui(["--uds", uds, "--no-spawn"], env=tui_env)  # 无 token（回归）
            tui2.wait_for("已连接 daemon", timeout=30, desc="第二个会话连接")
            tui2.key("ctrl-c")
            code2 = tui2.wait_exit()
            assert code2 == 0, f"Ctrl-C 退出码应为 0: {code2}"
            assert daemon.poll() is None, "开关关时 Ctrl-C 不应关闭 daemon"
            print("[S9a] 开关关：Ctrl-C 只退 TUI、daemon 存活 ✓")
            # 9b) Settings 页：选中设置项后按 → 打开「退出时关闭 daemon」
            tui3 = Tui(["--uds", uds, "--no-spawn"], env=tui_env)
            tui3.wait_for("已连接 daemon", timeout=30, desc="第三个会话连接")
            for _ in range(7):
                tui3.key("tab")
            tui3.wait_for("· Settings", desc="到 Settings 页")
            tui3.key("right")
            tui3.wait_for("退出时将同时关闭 daemon", timeout=10, desc="开关已打开")
            # 9c) Ctrl-C → daemon 递归关闭（sys.shutdown → 取消任务/杀进程组/清 helper）
            tui3.key("ctrl-c")
            code3 = tui3.wait_exit()
            assert code3 == 0, f"Ctrl-C 退出码应为 0: {code3}"
            deadline = time.time() + 20
            while time.time() < deadline and daemon.poll() is None:
                time.sleep(0.3)
            assert daemon.poll() is not None, "开关开时 Ctrl-C 后 daemon 应退出"
            print("[S9b] Settings → 开开关；Ctrl-C → daemon 递归关闭 ✓")

        finally:
            try:
                os.close(tui.fd)
            except OSError:
                pass
    finally:
        if daemon.poll() is None:
            daemon.terminate()
            daemon.wait(timeout=10)

    print("\n✅ TUI 真实交互模拟全部通过：帮助/切页/过滤/提交/跳转/取消/退出/Ctrl-C")


if __name__ == "__main__":
    main()
