# xtbp-parse 解析器 fixtures

本目录存放解析器的实测 fixtures，与 `crates/xtbp-parse/tests/parse_fixtures.rs` 一一对应。
所有命令在 conda 环境 `xtbp` 下执行，生成时间 2026-08-20。

## 工具版本

| 工具 | 版本 |
|---|---|
| xtb | 6.7.1 (edcfbbe) |
| crest | 3.0.2 (6914a25) |
| xtb4stda | 1.0 |
| stda | 1.6.1 |

## fixture 1：`water_xtb_json.out`

```bash
source /opt/miniforge3/etc/profile.d/conda.sh && conda activate xtbp
ulimit -s unlimited; OMP_NUM_THREADS=2
cp tests/chem/water.xyz /tmp/xtb-run/
xtb water.xyz --gfn 2 --json
```

**重要差异**：xtb 6.7.1 的 `--json` 不再把 JSON 打印到 stdout，而是写文件
`xtbout.json`；stdout 仍是人类可读输出。因此本 fixture 的内容即 `xtbout.json`
（机器可读 JSON），解析器 `xtb-json` 的输入就是这段 JSON 内容。

实测关键数值：total energy −5.07020801 Eh、electronic energy −5.10166616 Eh、
HOMO-LUMO gap 14.15968868 eV、dipole 矢量 `[-0.00289980, -0.89979983, 0.0]`
（a.u.，模 ≈ 0.8998 a.u. → 2.287 Debye）。注意 6.7.1 的 JSON 里**没有**
`gradient norm` 字段（梯度范数只在人类可读 SUMMARY 里），故解析器把
`gradient_norm` 视为可选。

## fixture 2：`ethanol_crest_property.json`

```bash
source /opt/miniforge3/etc/profile.d/conda.sh && conda activate xtbp
ulimit -s unlimited; OMP_NUM_THREADS=4
cp tests/chem/ethanol.xyz /tmp/xtb-run/
crest ethanol.xyz --gfn 2 --nconf 20 --T 4   # 约 11.7 s，CREST terminated normally
```

**重要差异**：crest 3.0.2 的基础构象搜索**不再生成** `crest_property.json`
（那是 crest 2.x 的产物），而是写 `crest.energies`（相对能量 kcal/mol）、
`crest_conformers.xyz`（带能量的构象）并在 stdout 的 "Final Ensemble
Information" 段打印每个构象的能量/权重/布居。

本 fixture 按 crest 2.x `crest_property.json` 的字段习惯（`Etot`=总能量 Eh、
`Erel`=相对能量 kcal/mol、`weight`=单重简并权重、`pop`=构象布居）从 crest
3.0.2 实测结果重建，数据来源：

- `crest.energies`：0.000 / 1.551 / 2.512 kcal/mol（3 个唯一构象）
- stdout "Final Ensemble Information"：Etot −11.39434 / −11.39187 / −11.39034 Eh；
  pop 0.96250 / 0.03518 / 0.00232（构象 1 含 6 个简并旋转异构体、构象 2 含 3 个、
  构象 3 含 1 个）
- `ensemble_energies.log`：精确到 10⁻⁹ Eh 的各构象能量

解析器 `crest-property` 读取 `conformers` 数组的 `Etot`（→ `conf_energy_{i}`）
与 `pop`（→ `conf_population_{i}`），按能量升序编号，并输出 `n_conformers`。

## fixture 3：`benzene_tda.dat`

```bash
source /opt/miniforge3/etc/profile.d/conda.sh && conda activate xtbp
export XTB4STDAHOME=~/opt/xtb4stda-1.0
ulimit -s unlimited; OMP_NUM_THREADS=4 MKL_NUM_THREADS=4
cp tests/chem/benzene.xyz /tmp/xtb-run/
xtb benzene.xyz --gfn 2 --opt tight          # 得 xtbopt.xyz
~/opt/xtb4stda-1.0/bin/xtb4stda xtbopt.xyz   # 生成 wfn.xtb
~/opt/stda-1.6.1/bin/stda -xtb wfn.xtb        # 生成 tda.dat + stdout 跃迁表
```

**重要差异**：stda 1.6.1 的 `tda.dat` 是 DATXY 展宽谱数据
（`NM/VELO/MMASS/LFAKTOR/.../DATXY` 段，每行只有 `state  eV  4 个光学响应量`），
**不含**波长 nm、振子强度 fL、跃迁轨道标记。带这些列的人类可读跃迁表
（`excitation energies, transition moments and TDA amplitudes` 段）打印在
**stdout**。因此本 fixture 保存的是 stdout 中这段跃迁表文本（含 header 与 3 行
跃迁），解析器 `tda-dat` 针对此格式。

实测跃迁表：

| state | eV | nm | fL | 主要跃迁（|系数|≥0.5） |
|---|---|---|---|---|
| 1 | 5.181 | 239.3 | 0.0000 | 10→12 / 9→11 |
| 2 | 6.630 | 187.0 | 0.0000 | 10→13 |
| 3 | 6.630 | 187.0 | 0.0000 | 9→13 |

态 1 振荡强度为 0 符合苯的对称禁阻跃迁（定性正确）。`sTDA done.` 正常结束。

## fixture 4：`water.xyz`

直接复制 `tests/chem/water.xyz`（3 原子水分子）。用于 `xyz` 解析器：
两行头 + 3 行坐标，`n_atoms = 3`。
