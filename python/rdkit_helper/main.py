#!/usr/bin/env python3
"""RDKit helper 常驻进程（设计文档 §4.1：stdio 换行 JSON 协议 v1）。

职责：SMILES → 3D 构型（ETKDGv3 嵌入 + MMFF/UFF 预优化）→ InChIKey + xyz。
由 daemon 拉起并看门：stdin 关闭即退出；RDKit 导入耗时只付一次。

仅依赖 rdkit + 标准库（规划文档 §1.2 helper 依赖规则），禁止 numpy 等额外依赖。

协议（每行一个 JSON 请求 / 一行 JSON 响应）：

    请求：{"id": 1, "op": "gen3d", "smiles": "CCO", "charge": 0, "mult": 1}
    成功：{"id": 1, "ok": true,  "result": {...}}
    失败：{"id": 1, "ok": false, "error": {"code": "...", "message": "..."}}

ops：ping / version / gen3d。
"""

from __future__ import annotations

import json
import sys

# 在模块顶层导入：常驻进程只付一次 RDKit 导入开销。
from rdkit import Chem
from rdkit.Chem import AllChem

# 协议版本（与 daemon 侧 HelperClient 的协议约定一致）。
PROTOCOL_VERSION = 1

# ETKDGv3 固定随机种子：使同一 SMILES 的构型可复现（利于幂等去重与目录即真相）。
_EMBED_SEED = 0xF00D

# 错误码（与 xtbp_core::job::error_codes 保持一致）。
ERR_INVALID_SMILES = "RDKIT_INVALID_SMILES"
ERR_PROTOCOL = "PROTOCOL_ERROR"
ERR_UNKNOWN_OP = "UNKNOWN_OP"


def rdkit_version() -> str:
    """RDKit 版本字符串（如 "2026.03.5"）。"""
    return Chem.rdBase.rdkitVersion


def emit(obj: dict) -> None:
    """写一行 JSON 响应并立即 flush（stdout 为管道时必须显式 flush，否则会死锁）。"""
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def ok(req_id, result: dict) -> dict:
    return {"id": req_id, "ok": True, "result": result}


def err(req_id, code: str, message: str) -> dict:
    return {"id": req_id, "ok": False, "error": {"code": code, "message": message}}


def handle(req: dict) -> dict:
    """按 op 分发单个请求。任何异常都不应逃逸到主循环（否则进程崩溃）。"""
    req_id = req.get("id")
    op = req.get("op")
    if op == "ping":
        return ok(req_id, {"pong": True, "version": str(PROTOCOL_VERSION)})
    if op == "version":
        return ok(req_id, {"protocol": PROTOCOL_VERSION, "rdkit": rdkit_version()})
    if op == "gen3d":
        return gen3d(req_id, req)
    return err(req_id, ERR_UNKNOWN_OP, f"未知 op: {op!r}")


def _optimize(mol, fn) -> bool:
    """调用优化器并兼容新旧返回（老版 int(0=成功)，新版 (not_converged, energy)）。

    返回是否收敛；任何异常（如该分子无 MMFF/UFF 力场参数）都视为失败。
    """
    try:
        res = fn(mol)
    except Exception:
        return False
    if isinstance(res, tuple):
        return res[0] == 0
    return res == 0


def gen3d(req_id, req: dict) -> dict:
    """SMILES → 3D 构型：MolFromSmiles → AddHs → EmbedMolecule(ETKDGv3)
    → MMFFOptimizeMolecule（失败退回 UFF）→ InChIKey → MolToXYZBlock。

    解析失败 / 价态错误 / 嵌入失败一律返回 RDKIT_INVALID_SMILES 结构化错误。
    """
    smiles = req.get("smiles")
    if not isinstance(smiles, str) or not smiles.strip():
        return err(req_id, ERR_INVALID_SMILES, "缺少或空 SMILES")

    # charge / mult 目前不改变构型（电荷由 SMILES 自身或 xtb flag 表达），
    # 仅作为请求契约保留，便于后续扩展与一致性校验。
    charge = req.get("charge", 0)
    mult = req.get("mult", 1)

    try:
        mol = Chem.MolFromSmiles(smiles)
    except Exception as exc:  # 罕见：RDKit 内部异常，仍按无效 SMILES 归类
        return err(req_id, ERR_INVALID_SMILES, f"SMILES 解析异常: {exc}")

    if mol is None:
        return err(req_id, ERR_INVALID_SMILES, f"无效 SMILES: {smiles}")

    try:
        mol = Chem.AddHs(mol)
        params = AllChem.ETKDGv3()
        params.randomSeed = _EMBED_SEED
        status = AllChem.EmbedMolecule(mol, params)
        if status != 0:
            return err(req_id, ERR_INVALID_SMILES, f"ETKDGv3 构型生成失败: {smiles}")

        warnings: list[str] = []
        # MMFF 优先；失败退回 UFF；两者都失败则降级返回 ETKDG 原始构型并告警。
        if not _optimize(mol, AllChem.MMFFOptimizeMolecule):
            if not _optimize(mol, AllChem.UFFOptimizeMolecule):
                warnings.append("MMFF 与 UFF 预优化均失败，返回 ETKDG 原始构型")
            else:
                warnings.append("MMFF 预优化失败，回退 UFF")

        inchikey = Chem.MolToInchiKey(mol)
        # 用 SMILES 作为 xyz 注释行标题，便于人工查看与追溯。
        mol.SetProp("_Name", smiles)
        xyz = Chem.MolToXYZBlock(mol)
        n_atoms = mol.GetNumAtoms()

        return ok(
            req_id,
            {
                "inchikey": inchikey,
                "xyz": xyz,
                "n_atoms": n_atoms,
                "warnings": warnings,
            },
        )
    except Exception as exc:
        return err(req_id, ERR_INVALID_SMILES, f"3D 生成异常: {exc}")


def main() -> int:
    # 就绪消息：stderr 一行，含协议版本，供 daemon 日志判读；不污染 stdout 协议流。
    print(
        f"rdkit_helper ready: protocol={PROTOCOL_VERSION} rdkit={rdkit_version()}",
        file=sys.stderr,
        flush=True,
    )
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError as exc:
            emit(err(None, ERR_PROTOCOL, f"请求非 JSON: {exc}"))
            continue
        if not isinstance(req, dict):
            emit(err(None, ERR_PROTOCOL, "请求必须是 JSON 对象"))
            continue
        try:
            emit(handle(req))
        except Exception as exc:  # 兜底：单个请求出错不拖垮常驻进程
            emit(err(req.get("id"), ERR_PROTOCOL, f"内部错误: {exc}"))
    # stdin EOF → 退出（看门契约）。
    return 0


if __name__ == "__main__":
    sys.exit(main())
