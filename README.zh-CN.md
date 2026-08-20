# xTB-Pilot-TUI

[![CI](https://github.com/zhiyaosu103/xtb-pilot-tui/actions/workflows/ci.yml/badge.svg)](https://github.com/zhiyaosu103/xtb-pilot-tui/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#许可证)
[![Rust 1.97.1](https://img.shields.io/badge/rust-1.97.1-orange.svg)](Cargo.toml)

[English](README.md) · **中文**

xTB-Pilot-TUI 是一个基于 Rust 开发的计算编排工具与 TUI 界面，用于在 WSL / Linux 环境下调度与管理 **xtb 计算化学工具链**（xtb、CREST、xTB4sTDA、sTDA）。

系统采用守护进程架构：用户可通过终端 TUI 交互监控，外部 Agent / 脚本可通过 TCP 接口（JSON-RPC 2.0）进行批量提交与结果提取。

## 核心特性

- **C/S 架构解耦**：后台 Daemon（`tokio`）负责任务调度与子进程管理，TUI 退出或关闭终端不会中断正在运行的计算任务。
- **双端交互支持**：
  - **交互式 TUI**（`ratatui`）：提供实时收敛曲线渲染、输出日志跟随（tail）、任务排队监控与快捷键交互。
  - **自动化 Agent 接口**：提供 TCP/NDJSON 接口（默认 `127.0.0.1:7700`），支持 JSON-Schema 自省与事件流订阅推送。
- **自动化组件探测**：启动时自动检索 PATH、Conda 环境及本地目录中的工具链与参数文件（如 sTDA 参数），支持多版本登记。
- **独立任务目录**：每个作业生成自洽的工作目录（包含输入文件、工作流快照与执行脚本 `cmd.txt`），便于脱离系统复现与排查。

## 架构

```
xtbp-tui (ratatui) ◄── UDS ──► xtbp-daemon (tokio) ◄── TCP 127.0.0.1:7700 ── Agent / Script
                                  │ 调度器 (xtbp-sched) / 工作流引擎 (xtbp-workflow)
                                  │ 任务装配 (xtbp-assemble) / 输出解析 (xtbp-parse)
                                  │ 存储与执行 (xtbp-store / xtbp-runner)
                                  ▼
                     xtb / CREST / xTB4sTDA / sTDA / RDKit
```

## 环境依赖

- **操作系统**：WSL2 (Ubuntu 22.04+) 或 Linux 原生环境（计算目录需位于 Linux 文件系统，如 `~`，不支持挂载盘 `/mnt/c`）
- **Rust 工具链**：1.97.1+
- **计算组件（Conda 环境推荐）**：
  - `xtb` 6.7.1+
  - `crest` 3.0.2+
  - `rdkit`
  - `xtb4stda` / `stda`（需配置参数文件 `.param_stda1.xtb`、`.param_stda2.xtb`）

## 安装

### 方式 A：预编译二进制（推荐）

从最新 [GitHub Release](https://github.com/zhiyaosu103/xtb-pilot-tui/releases) 下载 `xtbpilot-linux-x86_64.tar.gz`，然后：

```bash
tar -xzf xtbpilot-linux-x86_64.tar.gz
cp -r bin/. ~/.local/bin/
mkdir -p ~/.local/share/xtbpilot && cp -r templates ~/.local/share/xtbpilot/
```

### 方式 B：源码构建

```bash
# 1) 创建 Conda 环境（xtb / crest / rdkit）
conda env create -f environment.yml

# 2) 可选：xtb4stda / stda 二进制与参数文件（装至 ~/opt/xtb4stda-1.0）
./scripts/install-components.sh

# 3) 编译并安装 xtbp-tui / xtbp-daemon 至 ~/.local/bin
./scripts/install.sh
```

## 快速使用

### 1. 启动 TUI 界面

直接运行 `xtbp-tui`。若后台 Daemon 未运行，TUI 会自动拉起守护进程：

```bash
xtbp-tui
```

**常用快捷键：**

| 按键 | 说明 |
|---|---|
| `Tab` / `Shift+Tab` | 切换功能标签页 |
| `j` / `k` (或方向键) | 上下移动任务条目 |
| `Space` | 查看选中任务详情、日志 tail 与能量收敛曲线 |
| `s` | 弹出提交窗口（输入 SMILES 与选择工作流） |
| `/` | 过滤/搜索当前列表 |
| `c` | 取消选中任务 |
| `?` | 打开帮助面板 |
| `q` | 退出 TUI（后台任务继续运行） |

*注：如需禁用自动拉起 Daemon，可追加参数 `xtbp-tui --no-spawn`。*

### 2. 通过 Agent / 脚本调用 (TCP JSON-RPC)

Daemon 启动后在本地监听端口（默认 `127.0.0.1:7700`），认证令牌保存于 `~/.xtbpilot/agent.json`。

```python
# Python 客户端调用示例（测试用例提供最小实现）
python3 tests/agent_smoke.py <token>
```

查看 Daemon 导出的完整 API JSON-Schema：

```bash
xtbp-daemon api-schema
```

## 内置工作流

系统预置了常用的半经验计算流程（定义于 `templates/*.toml`），支持重复提交自动哈希去重：

| 标识 | 流程说明 | 产物/关注结果 |
|---|---|---|
| `opt` | 3D 构象生成 → GFN2-xTB 结构优化 | 优化坐标、基态能量 |
| `sp` | 单点能计算 (`--sp`) | 体系总能量、轨道能级 |
| `opt-freq` | 几何优化 + 谐振频率分析 (`--ohess`) | 零点能 (ZPE)、振动频率、热力学修正 |
| `conformer` | CREST 构象搜索 | 构象系综 (conformer ensemble) |
| `excited` | 结构优化 → xtb4stda → sTDA | 激发能、振子强度、展宽紫外光谱数据 |
| `redox` | 中性 / 阳离子 / 阴离子 三态优化 | 绝热/垂直电离能与电子亲和能 (IP/EA) |
| `reorg-4pt` | 四点法重组能计算 | 空穴/电子重组能 ($\lambda_h$, $\lambda_e$) |
| `solv-series` | 多溶剂 ALPB 连续单点计算 | 溶剂化自由能变化序列 |

## 开发与测试

```bash
# 运行全工作区单元测试（包含领域模型、存储、解析等）
cargo test --workspace

# 运行 RDKit Helper 协议测试
conda run -n xtbp python python/rdkit_helper/test_helper.py

# 运行端到端集成测试（需真实计算环境）
python3 tests/agent_smoke.py <token>       # Agent 接口与真实计算验证
python3 tests/tui_interaction.py          # 基于 PTY 的 TUI 交互会话测试
python3 tests/batch_parallel.py           # 批量高并发调度测试
```

## 规范与文档

- [架构与设计细节决策](docs/architecture-and-design-notes.md)
- [交互与终端规范](docs/engineering-spec.md)
- [贡献指南与开发流程](CONTRIBUTING.md)
- [变更记录](CHANGELOG.md)

## 许可证

本项目采用 [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE) 双重许可证授权。
