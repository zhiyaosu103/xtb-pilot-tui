# xTB-Pilot

[![CI](https://github.com/zhiyaosu103/xtb-pilot-tui/actions/workflows/ci.yml/badge.svg)](https://github.com/zhiyaosu103/xtb-pilot-tui/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#许可证)
[![Rust 1.97.1](https://img.shields.io/badge/rust-1.97.1-orange.svg)](Cargo.toml)

基于 Rust 的 TUI 应用：在 WSL 上编排 **xtb 及其生态组件**（xtb、CREST、xTB4sTDA + stda，可选 QCG / aISS）。
**人（TUI）和 agent（TCP 接口）都能方便地提交、监控、回收 xtb 家族计算**，并对结果做持久化与导出。
纯流水线工具：不做任何科学判断，只忠实执行与回收（设计文档 v0.2，本仓库已实现 P0–P6）。

## 架构一览（设计文档 §2.2 十 crate 布局）

```
xtbp-tui (ratatui) ◄─UDS─► xtbp-daemon (tokio) ◄─TCP 127.0.0.1:7700─ Windows agent
                              │ 调度器 xtbp-sched / 工作流引擎 xtbp-workflow
                              │ 组装 xtbp-assemble / 解析 xtbp-parse / 执行 xtbp-runner
                              │ SQLite + 文件仓 xtbp-store / 领域模型 xtbp-core
                              ▼
              xtb / crest / xtb4stda / stda / rdkit-helper（常驻进程）
```

- **TUI 只是 daemon 的客户端**：关 TUI 不中断计算，重连即恢复监控；
- **协议**：JSON-RPC 2.0，NDJSON over TCP（agent，`127.0.0.1:7700`）/ UDS（TUI），
  一行一个请求/响应；`job.events`/`queue.events` 订阅后服务器主动推流；
- **目录即真相**：每个任务一个自洽计算目录（input/ work/ output/），
  daemon 宕机后人可 `cd` 进去手动重跑 `cmd.txt`。

## 安装与使用（面向人类用户，即输即用）

```bash
# 一次性安装（release 构建 → ~/.local/bin，本机已在 PATH 中）：
./scripts/install.sh

# 之后在 WSL 终端的任意目录直接输入即可：
xtbp-tui
# → daemon 未运行时会自动拉起（setsid 脱离终端，关闭 TUI/终端后计算不中断）；
#   再次输入 xtbp-tui 秒连已有 daemon。Vim 键位：Tab 切页 / hjkl 移动 /
#   Space 选中看 tail 与收敛曲线 / s 提交 / ? 帮助 / q 退出。
# 老派用法（不自动拉起）：xtbp-tui --no-spawn

# agent 侧（Windows 裸 socket 同构；token 在 ~/.xtbpilot/agent.json）：
python3 tests/agent_smoke.py <token>
```

自动拉起特性：

- TUI 启动时探测 UDS，连不上即拉起 `xtbp-daemon`（PATH 查找，可用
  `--daemon <路径>` 指定）后自动重连，状态行提示「daemon 已自动拉起」；
- 拉起的 daemon 继承终端环境，并自动探测 `XTB4STDAHOME`（
  `~/opt/xtb4stda-1.0` 存在即启用）与 HOME 侧 sTDA 参数文件——**sTDA 工作流
  开箱即用，无需手动 export**；
- daemon 独立存活：关 TUI、关终端都不中断计算（§2.1 关键决策）。

## 快速开始（开发模式）

```bash
# 环境（已在本机验证）：conda env `xtbp`（xtb 6.7.1 / crest 3.0.2 / rdkit 2026.03.5）
# + 回退二进制 xtb4stda / stda（~/opt/，见下「组件登记」）

cargo build --workspace

# 1) 启动 daemon（agent 接口 + TUI 接口 + 自动组件发现 + token 落盘）
./target/debug/xtbp-daemon
#   → 监听 127.0.0.1:7700；token 写入 ~/.xtbpilot/agent.json（Windows 侧经 \\wsl$ 读取）

# 2) TUI
./target/debug/xtbp-tui

# 3) agent（裸 socket，Windows 侧同构）
python3 tests/agent_smoke.py <token>
```

## 验收状态（设计文档 §5）

| 里程碑 | 验收 | 状态 |
|---|---|---|
| P0 骨架 | workspace、daemon/TUI 经 UDS 互通、SQLite 迁移、`sys.health` 往返 | ✅ |
| P1 最小闭环 | runner + 调度 + xtb 优化 + TUI 提交/监控/结果；苯 GFN2 优化闭环 | ✅ 苯能量 −15.879641 Eh（参考 −15.8796），tail 351 行 + 收敛曲线 |
| P2 组装与解析 | RDKit helper、计算目录组装、解析器 + 实测 fixtures | ✅ `SMILES → 目录 → 结果入库`；15 解析测试 + insta 快照 |
| P3 工作流 | 7 内置模板 + DAG；4CzIPN `excited` 全链产出光谱 | ✅（见下） |
| P4 agent 接口 | TCP NDJSON、token、订阅、schema 导出；裸 socket 闭环 | ✅ `tests/agent_smoke.py` 全通过；`xtbp-daemon api-schema` 输出全部方法 JSON-Schema |
| P5 导出与硬化 | CSV/JSON/zip 导出、资源令牌、WSL 自检、kill -9 红线 | ✅ `tests/redline_test.py` 全通过 |
| P6 TUI 交互 | PTY 模拟真实用户会话 + 批处理并行调度实弹 | ✅ `tests/tui_interaction.py` 9 场景全过；`tests/batch_parallel.py` 21 分子 × 3 工作流 = 63 任务（见下） |

**红线实测**：`kill -9` daemon 后重启——数据库完好（WAL）、无僵尸进程、任务状态可解释
（Running/Queued → `interrupted`，可取消收口）；`--data-dir /mnt/c/...` 一律拒绝启动。

**开发红线全绿**：`cargo clippy --workspace --all-targets -- -D warnings`、
`cargo fmt --check`、workspace 全部 140+ 测试（Rust 133 + Python helper 6）每次提交前通过。

## 组件登记（InstanceRegistry，设计文档 §3.2）

daemon 启动时自动发现并登记（PATH → conda env → `~/opt/<name>-*/bin/<name>`），
登记表 `~/.local/share/xtbpilot/registry.toml`（TOML，多版本并存，任务可 pin 版本）。
conda-forge 缺失的 xtb4stda / stda 用 grimme-lab 预编译二进制 + 参数文件回退：

```bash
# 二进制：https://github.com/grimme-lab/xtb4stda/releases/download/v1.0/{xtb4stda,stda_v1.6.1}
# 参数文件（缺一不可）：.param_stda1.xtb 与 .param_stda2.xtb 放入 XTB4STDAHOME
```

版本探测失败的组件（如 xtb4stda 只打 banner）登记为 `0.0.0`，仍可正常执行。

## 内置工作流（`templates/*.toml`）

`opt`（gen3d→GFN2 opt）· `sp`（gen3d→GFN2 单点能 --sp）· `conformer`（CREST 构象）·
`opt-freq`（--ohess 频率）· `excited`（opt→xtb4stda→stda，展宽谱）· `redox`（三态 opt）·
`reorg-4pt`（四点法 λ_h/λ_e）· `solv-series`（多溶剂 ALPB 单点，`params.extra.solvents` 展开）。
提交形态：`job.submit { molecule_id|smiles, workflow, params?, priority?, dry_run? }`；
同 (SMILES, 工作流, 参数) 内容哈希幂等——重复提交直接复用结果。

## TUI 交互与批处理演练（P6）

- **`tests/tui_interaction.py`**：PTY 中拉起真实 `xtbp-tui`，注入键序列模拟完整用户会话
  （帮助 `?` / Tab 八页切换 / `/` 过滤 / `s` 输入 SMILES 提交 / `j k`、`Home End`、
  `PgUp PgDn` 移动与翻页 / `Space` 详情与 tail / `c` 取消排队与运行中任务 /
  `q` 退出且 daemon 独立存活 / `Ctrl-C` 退出），内置 ANSI 网格仿真器做屏幕断言。
  曾揪出并修复：Ctrl-C 未实现（帮助文案与行为不符）、过滤态选中索引错位
  （详情/取消打到错误任务）、`q` 退出后进程僵死（键盘读取任务阻塞 tokio worker
  导致 Runtime::drop 等待）、任务列表混入工作流子步骤（对其按 `c` 会失败整条工作流）。
- **`tests/batch_parallel.py`**：21 个 CSD 含硼分子 × `sp`/`excited`/`reorg-4pt` = 63 任务
  一次全量提交（隔离 daemon、4 并发槽、优先级 0/1/2），事件流统计峰值并发与
  优先级序，真实 TUI 全程在线采样 Dashboard 的 q/r 统计，终态后汇总
  每分子 E_sp / E_vert / λ_h / λ_e 到 `results/batch-<ts>/summary.md`。

## 输出解析（设计文档 §4.2 脆弱性对策）

优先机器可读输出：xtb 6.7.1 的 `--json`（写 `xtbout.json`）、crest 3.0 的能量文件、
stda 的跃迁表（在 stdout，tda.dat 是 DATXY 谱数据）。实测 fixture 与校准说明见
`crates/xtbp-parse/tests/fixtures/README.md`。**解析失败 ≠ 任务失败**：标记
`parse_degraded`、保留原始文件、TUI 显示 ⚠；所有结果带 `method_tier: screening` 声明。

## 测试

```bash
cargo test --workspace                 # 领域/存储/协议/调度/解析/渲染 等 140+ 测试
conda run -n xtbp python python/rdkit_helper/test_helper.py   # helper 协议 6 测试
python3 tests/agent_smoke.py <token>   # P4 端到端（真实 xtb 计算）
python3 tests/redline_test.py          # P5 红线（kill -9 / /mnt/c，隔离实例）
python3 tests/tui_interaction.py       # P6 TUI 真实交互模拟（PTY + 屏幕断言）
python3 tests/batch_parallel.py        # P6 批处理并行调度实弹（63 任务 + TUI 监控）
```

## 环境基线

| 组件 | 版本 | 备注 |
|---|---|---|
| WSL2 Ubuntu | 22.04+（本机 26.04） | 工作目录严禁 /mnt/c（daemon 启动硬校验） |
| conda / mamba | 26.3.2 / 2.5.0 | 环境定义 `environment.yml` / 锁定 `environment.lock.yml` |
| rustc / cargo / rustup | 1.97.1 | edition 2024；clippy -D warnings / fmt --check 全绿 |
| xtb / crest / rdkit | 6.7.1 / 3.0.2 / 2026.03.5 | conda env `xtbp` |

子进程环境由 runner 注入（OMP/MKL/OPENBLAS 线程数 + `ulimit -s unlimited` wrapper），
不依赖全局环境；时钟一律单调时钟（容忍 Windows 睡眠跳变）。

## 已知边界（与设计文档一致）

- 断点续算：崩溃后任务标记 `interrupted` 可解释，自动续算（xtbopt.coord / CREST 断点）
  留待后续里程碑；
- TUI 配置页展示只读（配置热重载待后续）；MCP 预留为 xtbp-api 薄封装扩展点；
- QCG / aISS 只登记实例不接工作流。

## 参与开发

- 开发红线、分支/PR 流程与端到端测试清单见 [CONTRIBUTING.md](CONTRIBUTING.md)；
- 里程碑变更记录见 [CHANGELOG.md](CHANGELOG.md)；
- CI（`cargo fmt` / `clippy -D warnings` / workspace 测试 / rdkit_helper 协议测试）
  定义于 `.github/workflows/ci.yml`。

## 许可证

MIT OR Apache-2.0（双许可，见 [LICENSE-MIT](LICENSE-MIT) 与 [LICENSE-APACHE](LICENSE-APACHE)）。
