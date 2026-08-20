#!/usr/bin/env python3
"""HelperClient 测试用假 helper（纯标准库，无 rdkit 依赖）。

读行回显 ping / gen3d 假数据；对 smiles="bad" 返回 RDKIT_INVALID_SMILES，
用于验证 HelperClient 把该错误码映射为 AssembleError::InvalidSmiles。

用法：HelperClient 测试用 `conda run -n xtbp python fake_helper.py` 拉起。
"""

from __future__ import annotations

import json
import sys

# 固定假数据，测试据此精确断言。
FAKE_INCHIKEY = "FAKEINCHIKEY-AAAAAAAAAA-N"
FAKE_XYZ = "3\nfake\nC 0.000000 0.000000 0.000000\nH 1.000000 0.000000 0.000000\nH -0.500000 0.800000 0.000000\n"


def emit(obj: dict) -> None:
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def main() -> int:
    print("fake_helper ready: protocol=1", file=sys.stderr, flush=True)
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            emit({"id": None, "ok": False, "error": {"code": "PROTOCOL_ERROR", "message": "bad json"}})
            continue
        op = req.get("op")
        rid = req.get("id")
        if op == "ping":
            emit({"id": rid, "ok": True, "result": {"pong": True, "version": "1"}})
        elif op == "version":
            emit({"id": rid, "ok": True, "result": {"protocol": 1, "rdkit": "fake"}})
        elif op == "gen3d":
            smiles = req.get("smiles", "")
            if smiles == "bad":
                emit({"id": rid, "ok": False, "error": {"code": "RDKIT_INVALID_SMILES", "message": f"无效 SMILES: {smiles}"}})
            else:
                emit({"id": rid, "ok": True, "result": {
                    "inchikey": FAKE_INCHIKEY,
                    "xyz": FAKE_XYZ,
                    "n_atoms": 3,
                    "warnings": ["假数据警告"],
                }})
        else:
            emit({"id": rid, "ok": False, "error": {"code": "UNKNOWN_OP", "message": f"未知 op: {op!r}"}})
    return 0


if __name__ == "__main__":
    sys.exit(main())
