#!/usr/bin/env python3
"""rdkit_helper 协议冒烟测试（标准库 unittest，直接 `python test_helper.py` 可跑）。

用 subprocess 拉起 main.py，验证：
- 启动就绪消息（stderr，含协议版本）；
- ping 往返；
- version 返回协议版本与 rdkit 版本字符串；
- gen3d 好 SMILES（CCO）返回正确 inchikey / n_atoms / xyz；
- gen3d 坏 SMILES 返回结构化错误 RDKIT_INVALID_SMILES；
- stdin EOF 后进程退出（看门契约）。

仅依赖标准库 + 本目录的 main.py（main.py 内部用 rdkit，需在 xtbp 环境运行）：
    conda run -n xtbp python python/rdkit_helper/test_helper.py
"""

from __future__ import annotations

import json
import subprocess
import sys
import threading
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
MAIN = HERE / "main.py"
PYTHON = sys.executable  # conda run -n xtbp python ... 时即 xtbp 环境的解释器


class HelperProtocolTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.proc = subprocess.Popen(
            [PYTHON, str(MAIN)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        # 后台线程排空 stderr（避免 RDKit 日志填满管道造成死锁），并收集就绪消息。
        cls.stderr_lines: list[str] = []
        cls._stderr_done = threading.Event()

        def drain() -> None:
            assert cls.proc.stderr is not None
            for line in cls.proc.stderr:
                cls.stderr_lines.append(line.rstrip("\n"))
            cls._stderr_done.set()

        cls._drain_thread = threading.Thread(target=drain, daemon=True)
        cls._drain_thread.start()

    @classmethod
    def tearDownClass(cls) -> None:
        # 关闭 stdin → helper 读 EOF → 自行退出（看门契约）。
        if cls.proc.stdin is not None:
            cls.proc.stdin.close()
        cls.proc.wait(timeout=30)

    def request(self, req: dict) -> dict:
        assert self.proc.stdin is not None and self.proc.stdout is not None
        self.proc.stdin.write(json.dumps(req) + "\n")
        self.proc.stdin.flush()
        line = self.proc.stdout.readline()
        if not line:
            self.fail("helper stdout 意外关闭（进程可能已退出）")
        return json.loads(line)

    def test_ready_message_on_stderr(self) -> None:
        # 就绪消息应在启动后打印在 stderr，含协议版本。
        deadline = 5.0
        import time

        start = time.monotonic()
        while time.monotonic() - start < deadline:
            if any("ready" in l for l in self.stderr_lines):
                break
            time.sleep(0.05)
        ready = next((l for l in self.stderr_lines if "ready" in l), None)
        self.assertIsNotNone(ready, "未观察到 stderr 就绪消息")
        self.assertIn("protocol=1", ready)

    def test_ping_roundtrip(self) -> None:
        resp = self.request({"id": 1, "op": "ping"})
        self.assertTrue(resp["ok"])
        self.assertEqual(resp["id"], 1)
        self.assertTrue(resp["result"]["pong"])
        self.assertEqual(resp["result"]["version"], "1")

    def test_version_op(self) -> None:
        resp = self.request({"id": 2, "op": "version"})
        self.assertTrue(resp["ok"])
        self.assertEqual(resp["result"]["protocol"], 1)
        # rdkit 版本字符串形如 "2026.03.5"，至少含数字。
        self.assertRegex(resp["result"]["rdkit"], r"\d")

    def test_gen3d_good_smiles(self) -> None:
        resp = self.request({"id": 3, "op": "gen3d", "smiles": "CCO", "charge": 0, "mult": 1})
        self.assertTrue(resp["ok"], f"gen3d 应成功，实际: {resp}")
        result = resp["result"]
        # 乙醇的 InChIKey（构型无关，仅由连通性决定，可精确断言）。
        self.assertEqual(result["inchikey"], "LFQSCWFLJHTTHZ-UHFFFAOYSA-N")
        self.assertEqual(result["n_atoms"], 9)
        # xyz 文本以原子数行开头，含原子坐标行。
        self.assertTrue(result["xyz"].startswith("9\n"))
        self.assertIn("\nC ", result["xyz"])
        self.assertIsInstance(result["warnings"], list)

    def test_gen3d_bad_smiles(self) -> None:
        resp = self.request({"id": 4, "op": "gen3d", "smiles": "bad_smiles", "charge": 0, "mult": 1})
        self.assertFalse(resp["ok"])
        self.assertEqual(resp["error"]["code"], "RDKIT_INVALID_SMILES")
        self.assertIn("bad_smiles", resp["error"]["message"])

    def test_unknown_op(self) -> None:
        resp = self.request({"id": 5, "op": "nope"})
        self.assertFalse(resp["ok"])
        self.assertEqual(resp["error"]["code"], "UNKNOWN_OP")


if __name__ == "__main__":
    unittest.main(verbosity=2)
