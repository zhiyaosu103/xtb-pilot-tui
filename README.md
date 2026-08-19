# xTB-Pilot

依赖规划文档 v0.2 的环境落地实现。本仓库包含 conda 环境定义（Python 侧）
与 Rust workspace（守护进程 / 接口 / TUI）。

## 环境基线（已在本机验证）

| 组件 | 版本 | 备注 |
|---|---|---|
| WSL2 Ubuntu | 22.04+（本机 26.04） | |
| conda / mamba | 26.3.2 / 2.5.0 | conda-forge 单渠道 |
| rustc / cargo / rustup | 1.97.1 / 1.97.1 / 1.29.0 | edition 2024；clippy + rustfmt 已装 |

## 1. Python 侧（conda 环境 `xtbp`）

```bash
mamba env create -f environment.yml     # 一键建环境（仅 conda-forge）
conda activate xtbp
python -c "import rdkit; print(rdkit.__version__)"   # 注意：新版 rdkit 的 Chem 子模块无 __version__
python -c "import numpy; print(numpy.__version__)"
xtb --version && crest --version
```

已装版本（本机实测）：python 3.12.13、rdkit 2026.03.5、numpy 2.5.2、
xtb 6.7.1、crest 3.0.2。锁定文件为 `environment.lock.yml`（`conda env export --no-builds`），
交付环境以 lock 文件重建。

### P0 结论：xtb4stda / stda 渠道可用性

`mamba search xtb4stda -c conda-forge` 与 `mamba search stda -c conda-forge`
均返回 `No entries matching ...`；anaconda.org API（`api.anaconda.org/package/conda-forge/{stda,xtb4stda}`）
亦为 NOT_FOUND。**结论：conda-forge 无此二包，回退预案已执行。**

### xtb4stda / stda 二进制注册（回退预案）

预编译二进制来自 grimme-lab GitHub Releases（xtb4stda v1.0 的 Release 同时附带 stda v1.6.1），
已下载并放置于：

- `~/opt/xtb4stda-1.0/bin/xtb4stda`
- `~/opt/stda-1.6.1/bin/stda`

下载源（网络受限时经 `https://gh-proxy.org/` 前缀代理）：
`https://github.com/grimme-lab/xtb4stda/releases/download/v1.0/xtb4stda`
`https://github.com/grimme-lab/xtb4stda/releases/download/v1.0/stda_v1.6.1`

**InstanceRegistry 手动登记流程**（设计文档 §3.2 多版本机制原生支持此路径；
daemon 以登记表为准拉起子进程，不依赖 PATH 运气）：

```bash
# 1) 写入登记表（TOML，默认 ~/.local/share/xtbpilot/registry.toml）
cat > ~/.local/share/xtbpilot/registry.toml <<'EOF'
[[entries]]
name = "xtb4stda"
version = "1.0.0"
exe = "/home/<user>/opt/xtb4stda-1.0/bin/xtb4stda"
sha256 = "<sha256sum 输出>"
id = "<ulid>"
EOF
```

也可以用 `xtbp-core` 的 `InstanceRegistry::register(name, version, exe)` 编程登记：
它会校验文件存在、计算 sha256、生成 ULID，并支持 `save()/load()` 持久化，
`latest(name)` 取多版本中的最新版。运行时验证：

```bash
~/opt/xtb4stda-1.0/bin/xtb4stda   # 应打印 "xTB for sTDA" banner
~/opt/stda-1.6.1/bin/stda         # 应打印 sTDA banner
```

> 提示：若将来 conda-forge 上架 xtb4stda/stda，删除本回退并恢复
> `environment.yml` 中的对应行，重新导出 lock 即可。

### 运行时环境变量（子进程注入，非全局）

| 变量 | 作用 | 默认（`xtbp_core::config::Config`） |
|---|---|---|
| `OMP_NUM_THREADS` | xtb 并行线程 | 1（调度器按资源令牌下发） |
| `MKL_NUM_THREADS` / `OPENBLAS_NUM_THREADS` | 防 BLAS 线程超额订阅 | 1 |
| `ulimit -s unlimited` | xtb 栈需求 | runner 的 wrapper shell 设置 |

## 2. Rust 侧（cargo workspace）

```
Cargo.toml                     # [workspace.dependencies] 集中声明全部版本
crates/
  xtbp-core/   核心库：InstanceRegistry、配置、ID/版本/哈希、SQLite(迁移)/CSV
  xtbp-api/    接口层：NDJSON over TCP 手写帧与分发、schemars JSON-Schema
  xtbp-tui/    TUI（ratatui 0.29 + crossterm 0.28 + tui-input 0.14）
  xtbp-daemon/ 守护进程：tokio 运行时、CancellationToken 优雅停机、滚动日志
```

```bash
cargo build --workspace        # P0 验收：干净通过
cargo test  --workspace        # 单元测试 + insta 快照 + tokio test-util 时间控制
```

依赖治理（规划文档 §2.4）：新增依赖须在 PR 写明理由；CI 跑 `cargo deny check`
与 `cargo audit`；升级策略 patch 随时 / minor 随里程碑 / major 单独评估，
ratatui/crossterm/tui-input 三者必须同批升级。

## 3. P0 验收清单

- [x] `environment.yml` 一键建环境成功（`mamba env create -f environment.yml`）
- [x] `cargo build --workspace` 干净通过
- [x] `mamba search` 确认 xtb4stda/stda 渠道可用性并记录结论（不可用 → 已执行回退，
      二进制注册流程见上文 §1，已写入本 README）
- [x] `environment.lock.yml` 已生成入库
