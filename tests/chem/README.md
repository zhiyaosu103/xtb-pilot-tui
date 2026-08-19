# xTB-Pilot 化学计算冒烟测试套件

本目录用于验证本机化学计算链路可正常出结果（规划文档 P0 之后的冒烟验证）。
所有命令在 conda 环境 `xtbp` 下执行；`xtb4stda`/`stda` 为回退二进制（见根 README）。

## 分子

| 文件 | 分子 | 原子数 | 用途 |
|---|---|---|---|
| `ethanol.xyz` | 乙醇 C₂H₅OH | 9 | xtb 几何优化 + crest 构象搜索 |
| `benzene.xyz` | 苯 C₆H₆ | 12 | sTDA 激发态（禁阻跃迁可作定性判据） |
| `water.xyz` | 水 H₂O | 3 | 快速单点 |

由 `gen_mols.py` 生成（rdkit ETKDG 构型 + MMFF 预优化；仅 rdkit + 标准库，
符合规划文档 §1.2 的 helper 依赖规则）：

```bash
conda activate xtbp
python gen_mols.py
```

## 冒烟验证命令（2026-08-20 本机实测结果）

### 1. xtb 单点（GFN2-xTB）

```bash
ulimit -s unlimited; OMP_NUM_THREADS=2
xtb water.xyz --gfn 2
# TOTAL ENERGY = -5.0702 Eh，HOMO-LUMO GAP = 14.16 eV，normal termination
```

### 2. xtb 几何优化（GFN2-xTB, tight）

```bash
mkdir -p opt-ethanol && cd opt-ethanol && cp ../ethanol.xyz .
OMP_NUM_THREADS=4 xtb ethanol.xyz --gfn 2 --opt tight
# 优化后能量 -11.3943 Eh，gradient norm 1.2e-4，normal termination
```

苯同理：`-15.8796 Eh`（gnorm 6.2e-6）。

### 3. crest 构象搜索

```bash
mkdir -p crest-ethanol && cd crest-ethanol && cp ../ethanol.xyz .
OMP_NUM_THREADS=4 crest ethanol.xyz --gfn 2 --nconf 20 --T 4
# CREST terminated normally，12.2 s，45409 次 energy+grad 调用
```

### 4. sTDA 激发态（xtb4stda → stda）

先确认 `~/opt/xtb4stda-1.0/` 下有 `.param_stda1.xtb` 与 `.param_stda2.xtb`
（缺第二个会报 `no basis found for atom ... Z=`，见根 README）。

```bash
export XTB4STDAHOME=~/opt/xtb4stda-1.0
ulimit -s unlimited; export OMP_NUM_THREADS=4 MKL_NUM_THREADS=4
mkdir -p sTDA-benzene && cd sTDA-benzene && cp ../benzene-opt/xtbopt.xyz .
~/opt/xtb4stda-1.0/bin/xtb4stda xtbopt.xyz      # 生成 wfn.xtb（GFN 轨道）
~/opt/stda-1.6.1/bin/stda -xtb wfn.xtb          # -xtb 读 xtb 轨道
```

苯实测结果（sTDA-xTB / GFN 轨道）：

| 态 | eV | nm | fL | 主要跃迁 |
|---|---|---|---|---|
| 1 | 5.181 | 239.3 | 0.0000 | 10→12 / 9→11 |
| 2 | 6.630 | 187.0 | 0.0000 | 10→13 |
| 3 | 6.630 | 187.0 | 0.0000 | 9→13 |

态 1 振荡强度为 0 符合苯的对称禁阻跃迁（定性正确）。`sTDA done.` 正常结束。

## 结论

本机化学计算链路全部可用：rdkit 构型生成 → xtb 单点/优化 → crest 构象 →
xtb4stda 轨道 → stda 激发态。若需复现，依次执行上述命令即可。
