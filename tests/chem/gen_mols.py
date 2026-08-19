#!/usr/bin/env python3
"""生成化学冒烟测试分子（rdkit ETKDG 构型 + MMFF 预优化）。

仅使用 rdkit + 标准库（规划文档 §1.2 helper 依赖规则）。
输出：本目录下的 ethanol.xyz / benzene.xyz / water.xyz
"""
import sys
from pathlib import Path

from rdkit import Chem
from rdkit.Chem import AllChem

HERE = Path(__file__).resolve().parent
MOLS = {
    "ethanol": "CCO",        # 几何优化 + 构象搜索
    "benzene": "c1ccccc1",   # sTDA 激发态
    "water": "O",            # 快速单点
}


def gen_xyz(smiles: str) -> str:
    mol = Chem.AddHs(Chem.MolFromSmiles(smiles))
    if AllChem.EmbedMolecule(mol, AllChem.ETKDGv3()) != 0:
        raise RuntimeError(f"ETKDG 构型生成失败: {smiles}")
    AllChem.MMFFOptimizeMolecule(mol)
    conf = mol.GetConformer()
    lines = [str(mol.GetNumAtoms()), smiles]
    for atom in mol.GetAtoms():
        p = conf.GetAtomPosition(atom.GetIdx())
        lines.append(f"{atom.GetSymbol():2s} {p.x:14.6f} {p.y:14.6f} {p.z:14.6f}")
    return "\n".join(lines) + "\n"


def main() -> int:
    for name, smiles in MOLS.items():
        out = HERE / f"{name}.xyz"
        out.write_text(gen_xyz(smiles))
        print(f"written {out} ({smiles})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
