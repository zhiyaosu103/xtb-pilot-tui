# 贡献指南

欢迎参与 xTB-Pilot 开发。本仓库是一个**纯流水线工具**：不做任何科学判断，
只忠实执行与回收 xtb 家族计算。请先阅读 `README.md`（架构、验收状态）与
`CHANGELOG.md`，再开始改动。

## 环境准备

```bash
# 1) conda 环境（xtb / crest / rdkit 来自 conda-forge，环境名 xtbp）
conda env create -f environment.yml

# 2) 编译
cargo build --workspace

# 3) 可选：xtb4stda / stda 二进制回退（conda-forge 无条目）
#    从 grimme-lab GitHub Releases 下载预编译二进制放 ~/opt/<name>-<ver>/bin，
#    参数文件 .param_stda1.xtb / .param_stda2.xtb 放入 XTB4STDAHOME（见 README「组件登记」）
```

## 开发红线（提交前必须全绿）

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
conda run -n xtbp python python/rdkit_helper/test_helper.py   # helper 协议 6 测试
```

以下端到端测试需要真实计算环境，不在 CI 中运行，但**涉及对应模块的改动必须本地通过**：

```bash
python3 tests/agent_smoke.py <token>    # P4 agent 接口（真实 xtb 计算）
python3 tests/redline_test.py           # P5 红线（kill -9 / /mnt/c，隔离实例）
python3 tests/tui_interaction.py        # P6 TUI 真实交互（PTY + 屏幕断言）
python3 tests/batch_parallel.py         # P6 批处理实弹（63 任务，耗时较长）
```

## 工作流

- 新功能从 `main` 开分支，合并走 PR（CI 必须通过）；
- 提交信息遵循 Conventional Commits：`feat` / `fix` / `test` / `refactor` / `chore` / `docs`，
  中文描述，例如 `feat(tui): 八页 ratatui 界面——事件订阅驱动`；
- `Cargo.toml` 依赖治理：版本集中声明于 `[workspace.dependencies]`，成员 crate 一律
  `{ workspace = true }` 引用；**新增依赖必须在 PR 描述中写明理由**（规划文档 §2.4）；
- `Cargo.lock` 提交入库；CI 跑 `cargo deny check` 与 `cargo audit` 之前请自查；
- 新增/修改解析器必须附带**实测 fixtures + insta 快照**
  （`crates/xtbp-parse/tests/fixtures/`，`cargo insta review` 审查未接受快照）。

## 行为约定

- daemon 对 `--data-dir /mnt/c/...` 一律拒绝启动（WSL 边界，硬校验，不得放宽）；
- 解析失败 ≠ 任务失败：标记 `parse_degraded`、保留原始文件、TUI 显示 ⚠；
- 所有结果携带 `method_tier: screening` 声明；任务自洽目录（input/ work/ output/）是
  唯一真相，daemon 宕机后人可 `cd` 进去手动重跑 `cmd.txt`。
