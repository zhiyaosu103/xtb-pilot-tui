# xTB-Pilot 工程规范（交互与实现约定）

> 本文档沉淀 TUI/daemon 交互的实现约定。**新增交互功能前先读这里**；
> 与既有约定冲突的设计需先改规范再实现。

## 1. TUI 键位与交互约定

### 1.1 导航（全局）

| 键 | 语义 |
|---|---|
| `Tab` / `Shift+Tab` | 页面循环切换 |
| `j k` / `↑ ↓` | 列表移动选中 |
| `Home` / `End` | 列表首 / 尾 |
| `PgUp` / `PgDn` | 翻页（一页 10 行） |
| `/` | 过滤（Enter 应用，Esc 取消；输入模式内 Esc 取消） |
| `?` | 帮助（任意键关闭） |
| `q` / `Esc` | 退出 TUI（是否连带关闭 daemon 见 §1.3） |
| `Ctrl-C` | 退出 TUI（同 q；任何输入模式下都生效，优先级最高） |

### 1.2 可切换选项：一律用 ←/→ 切换（本规范核心约定）

**开关类 / 可枚举选项的切换统一使用左右方向键**，不占用字母键：

- Settings 页：`j/k` 上下移动选中设置项，**←/→ 切换该项的值**；
- 后续任何页面出现「开关 / 枚举选择」控件，一律遵循：
  选中（高亮）该选项 → 按 `←` / `→` 切换值；
- 禁止用 `空格` / `Enter` / 字母键切换开关（这些键留给提交、详情等
  既有语义；Workflows 页 `[` `]` 切换计算水平是历史例外，新控件不再增加
  字母键切换）。

### 1.3 退出语义（q / Esc / Ctrl-C）

- 由 Settings 页开关「退出时关闭 daemon」决定（默认关，持久化在
  `~/.local/share/xtbpilot/tui-settings.json`）：
  - **关（默认）**：退出只关 TUI；daemon 独立存活（§2.1 原设计），
    在最后一个任务完成（xtb 返回结果并入库）后空闲自毁；
  - **开**：退出时 TUI 先调用 `sys.shutdown`（daemon 取消全部任务、
    递归杀计算进程组与 RDKit helper）再退出；
- 两种模式下 TUI 均以退出码 0 结束。

## 2. daemon 生命周期约定

### 2.1 独立存活（原设计，保留）

TUI/终端关闭不中断计算（daemon setsid 脱离会话）。

### 2.2 空闲自毁（补充）

- daemon 曾有过任务（running/queued 出现过非零）→ 任务全部完成、
  持续空闲 `--idle-shutdown-secs`（默认 30，0=禁用）→ 自动优雅退出；
- 启动即空闲（从未有过任务）不触发自毁，避免「刚拉起就退出」；
- 自毁后 TUI/agent 下次操作会自动拉起新 daemon（TUI 侧 spawn 兜底）。

### 2.3 停机路径（统一）

`SIGTERM` / 终端 `Ctrl-C` / RPC `sys.shutdown` 共用同一条优雅停机路径：
调度器取消全部任务（runner 递归杀进程组）→ `HelperClient::kill_tree()`
（helper 以 setsid 启动为进程组首领，`kill(-pgid)` 带走 conda run 与
python 孙进程）→ shutdown token cancel → 进程退出。

## 3. 实现约定速查

- 键盘读取用 `std::thread`（阻塞式 crossterm 调用不能挂 tokio worker，
  否则 Runtime::drop 永不返回，q 后进程僵死）；
- 任务列表只展示工作流任务（父任务），子步骤（`parent_id` 非空）不展示；
  过滤态下选中索引一律基于过滤后列表；
- 事件总线可能 Lagged 丢事件：工作流引擎以 store 终态对账收口，
  任何「等待事件」的逻辑都必须有 store 侧兜底；
- UDS（TUI 通道）不鉴权，TCP（agent 通道）token 鉴权；
- 提交表单参数经 `Method` 模型单点生成命令行 flag（`xtb_flags`），
  禁止在模板/调用方散落 flag。
