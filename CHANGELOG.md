# Changelog

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。
本文件自首个公开提交起补记，条目按里程碑归组（对应 README「验收状态」P0–P6）。

## [Unreleased]

### Added

**P0 骨架与领域模型**

- workspace 重构为 10-crate 布局（`xtbp-core/store/runner/parse/workflow/sched/assemble/api/tui/daemon`），
  依赖版本集中于 `[workspace.dependencies]`（依赖治理 §2.4）；
- 领域模型：Molecule / Method / Job 状态机 / Workflow 模板 / Result，ULID 标识与内容哈希；
- SQLite 仓储层：全量迁移 + 内容寻址文件仓（输入/输出按哈希落盘）。

**P1 最小闭环**

- 子进程封装（`xtbp-runner`）：进程组 kill、超时/停滞检测、流式逐行回调；
- 调度器（`xtbp-sched`）：优先级队列、并发槽、内存令牌、退避重试、取消与崩溃恢复；
- TUI ↔ daemon 经 UDS 互通，`sys.health` 往返；苯 GFN2 优化闭环验收通过。

**P2 组装与解析**

- RDKit helper（`python/rdkit_helper`，协议式 gen3d）+ 计算目录组装；
- 输出解析器（xtb `--json` / crest 能量文件 / stda 跃迁表），实测 fixtures + insta 快照；
- `parse_degraded` 语义：解析失败 ≠ 任务失败，保留原始文件并标记 ⚠。

**P3 工作流**

- 7 内置模板（opt/sp/conformer/opt-freq/excited/redox/reorg-4pt/solv-series）+ DAG 引擎，
  `{nconf}/{sigma_ev}` 模板变量渲染；4CzIPN `excited` 全链产出光谱。

**P4 agent 接口**

- JSON-RPC 2.0 over TCP（127.0.0.1:7700，NDJSON）+ UDS（TUI），token 鉴权、
  订阅推送（`job.events`/`queue.events`）、JSON-Schema 导出（`api-schema` 子命令）；
- 裸 socket 闭环验收：`tests/agent_smoke.py` 全通过。

**P5 导出与硬化**

- CSV/JSON/zip 导出、资源令牌、WSL 自检、`--data-dir /mnt/c/...` 硬拒绝；
- `kill -9` 红线：数据库完好（WAL）、无僵尸进程、Running/Queued → `interrupted` 可解释收口。

**P6 TUI 交互与批处理**

- TUI 即输即用：用户级安装（`scripts/install.sh`）+ daemon 自动拉起（setsid 独立存活）、
  断线重连、Vim 键位、八页导航、Braille 收敛曲线、过滤/详情/取消；
- PTY 真实会话模拟（`tests/tui_interaction.py`，ANSI 网格屏幕断言）；
- 批处理并行调度实弹（`tests/batch_parallel.py`：21 分子 × 3 工作流 = 63 任务）。

### Fixed

- 事件总线 Lagged 丢 `Finished` 导致工作流卡死（引擎对账收口）；
- 引擎异常时父任务失败收口、xtb4stda 参数文件 HOME 兜底；
- tda 跃迁表解析排除 CSF 块，excited 回收 stdout 表；
- 工作流 `{nconf}/{sigma_ev}` 预替换后未传入渲染的回归；
- UDS 不鉴权、提交表单参数化、`.smi` 批量导入；
- Ctrl-C 递归全链退出（`sys.shutdown` RPC + helper 进程组清理）；
- runner 监督循环竞态：子进程退出瞬间的最后输出行丢失——泵任务尚未被调度时
  直接 `try_recv` 排空所致（CI 低核高争用实测必现，如 `env_injection_reaches_child`
  断言 `None`），改为边等泵边 `recv` 排空，并保留 2s 有界超时。
- 原生 Linux 上 `o` 键查看器不可用：WSL interop 路径失败时降级 `xdg-open`。

### Changed

- 批处理演练脚本以 store 轮询为准，修正报告键名；
- 引擎测试引入临时目录守卫。
- README 改为英文主文档 + `README.zh-CN.md` 中文副文档，GitHub 主页语言切换；
  设计决策与里程碑验收归档至 `docs/architecture-and-design-notes.md`。

### Added

- `.github/workflows/release.yml`：tag 触发构建 Linux x86_64 预编译包并发布
  GitHub Release（`bin/{xtbp-tui,xtbp-daemon}` + `templates/`）；
- `scripts/install-components.sh`：一键下载 xtb4stda / stda 二进制与参数文件；
- `install.sh` 与 daemon 模板目录解析：工作流模板落位
  `$XDG_DATA_HOME/xtbpilot/templates`，安装版不再依赖构建目录存活。

## 开发红线（每次提交前）

- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings` 全绿；
- workspace 全部测试通过（Rust 133 + Python helper 6）。
