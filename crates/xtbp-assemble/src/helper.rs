//! RDKit helper 常驻进程客户端（设计文档 §4.1：stdio 换行 JSON 协议 v1）。
//!
//! 拉起 `conda run -n xtbp python <main.py>` 常驻进程，避免每分子一次 Python 启动；
//! RDKit 导入耗时只付一次。stdin 关闭即 helper 退出（看门契约）。
//!
//! 协议：每行一个请求 JSON，响应一行 JSON，按 `id` 匹配（跳过迟到的旧响应）。
//! 参数以数组传递，禁止字符串拼接命令。

use crate::{AssembleError, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;
use tracing::{debug, warn};

/// 单次请求超时（秒）。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// gen3d 成功结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gen3dOutput {
    /// InChIKey（首段 14 字符 + 后缀，用于去重）。
    pub inchikey: String,
    /// xyz 文本块（含原子数首行与注释行）。
    pub xyz: String,
    /// 原子数（含氢）。
    pub n_atoms: usize,
    /// 降级警告（如 MMFF 回退 UFF）。
    pub warnings: Vec<String>,
}

/// RDKit helper 常驻进程客户端。
pub struct HelperClient {
    /// 子进程句柄。
    child: Child,
    /// stdin（请求写入端；None 表示已优雅关闭）。
    stdin: Option<ChildStdin>,
    /// stdout 逐行读取器（响应端）。
    stdout: BufReader<ChildStdout>,
    /// 下一个协议请求 id。
    next_id: u64,
    /// conda 可执行文件路径（ensure_alive 重启复用）。
    conda: PathBuf,
    /// helper 入口脚本路径（ensure_alive 重启复用）。
    main_py: PathBuf,
}

impl HelperClient {
    /// 拉起 helper：`[conda, "run", "--no-capture-output", "-n", "xtbp", "python", <main.py 路径>]`。
    ///
    /// main.py 路径 = `env!("CARGO_MANIFEST_DIR")/../../python/rdkit_helper/main.py`
    /// （canonicalize）。conda 探测见 [`Self::find_conda`]。
    pub async fn spawn() -> Result<Self> {
        let main_py = default_main_py()?;
        Self::spawn_with_main_py(&main_py).await
    }

    /// 以指定 python 脚本作为 helper 入口拉起（测试与自定义入口用）。
    pub async fn spawn_with_main_py(main_py: &Path) -> Result<Self> {
        let conda = Self::find_conda()?;
        Self::spawn_script(&conda, main_py).await
    }

    /// 探测 conda 可执行文件路径。
    ///
    /// 顺序：`$XTBP_CONDA` → 常见路径（`/opt/miniforge3/condabin/conda`、
    /// `~/miniforge3/condabin/conda`、`~/miniconda3/condabin/conda`）的 `is_file` 检查
    /// → 都不存在则 `Err(Rdkit{message})`。
    pub fn find_conda() -> Result<PathBuf> {
        // 1. 显式环境变量优先（测试 / 自定义安装）。
        if let Ok(p) = std::env::var("XTBP_CONDA") {
            let p = p.trim();
            if !p.is_empty() {
                return Ok(PathBuf::from(p));
            }
        }

        // 2. 常见安装路径（`which conda` 的常见落点）。
        let home = std::env::var("HOME").unwrap_or_default();
        let candidates = [
            PathBuf::from("/opt/miniforge3/condabin/conda"),
            PathBuf::from(format!("{home}/miniforge3/condabin/conda")),
            PathBuf::from(format!("{home}/miniconda3/condabin/conda")),
        ];
        for c in candidates {
            if c.is_file() {
                return Ok(c);
            }
        }

        Err(AssembleError::Rdkit {
            message: "未找到 conda 可执行文件（尝试 $XTBP_CONDA 与常见安装路径）".to_string(),
        })
    }

    /// 实际拉起子进程并装配 stdio / stderr 泵。
    async fn spawn_script(conda: &Path, main_py: &Path) -> Result<Self> {
        let (child, stdin, stdout) = spawn_child(conda, main_py).await?;
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout,
            next_id: 1,
            conda: conda.to_path_buf(),
            main_py: main_py.to_path_buf(),
        })
    }

    /// 一次请求（带 30s 超时）：注入唯一 id、写行、按 id 匹配读行（跳过不匹配响应）。
    ///
    /// 超时 / EOF / 写失败 / 读失败一律视为 helper 不可用，返回 [`AssembleError::HelperGone`]
    /// （超时时先杀死子进程，便于 [`Self::ensure_alive`] 自愈重启）。
    pub async fn request(&mut self, req: &serde_json::Value) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        // 注入协议 id（覆盖调用方传入值，保证唯一）；请求对象为调用方所有，克隆避免原地修改。
        let mut full = req.clone();
        full["id"] = json!(id);
        let line = serde_json::to_string(&full).map_err(|e| AssembleError::Serialize {
            message: format!("helper 请求序列化失败: {e}"),
        })?;

        // 写请求行（作用域隔离 stdin 的可变借用，避免与后续 stdout 读取冲突）。
        {
            let stdin = self.stdin.as_mut().ok_or(AssembleError::HelperGone)?;
            if let Err(e) = stdin.write_all(line.as_bytes()).await {
                warn!("rdkit helper stdin 写入失败（视为进程已死）: {e}");
                return Err(AssembleError::HelperGone);
            }
            if let Err(e) = stdin.write_all(b"\n").await {
                warn!("rdkit helper stdin 写入失败: {e}");
                return Err(AssembleError::HelperGone);
            }
            if let Err(e) = stdin.flush().await {
                warn!("rdkit helper stdin flush 失败: {e}");
                return Err(AssembleError::HelperGone);
            }
        }

        // 按 id 匹配读行：跳过迟到 / 不匹配 / 非 JSON 的响应行。
        loop {
            let mut line = String::new();
            let read = timeout(REQUEST_TIMEOUT, self.stdout.read_line(&mut line)).await;
            let n = match read {
                Ok(r) => r,
                Err(_) => {
                    warn!(
                        "rdkit helper 响应超时（>{}s），杀死子进程待重启",
                        REQUEST_TIMEOUT.as_secs()
                    );
                    let _ = self.child.start_kill();
                    return Err(AssembleError::HelperGone);
                }
            };
            let n = match n {
                Ok(n) => n,
                Err(e) => {
                    warn!("rdkit helper stdout 读取失败: {e}");
                    return Err(AssembleError::HelperGone);
                }
            };
            if n == 0 {
                warn!("rdkit helper stdout 关闭（进程已退出）");
                return Err(AssembleError::HelperGone);
            }
            let resp: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    warn!("rdkit helper 响应非 JSON，跳过: {e}");
                    continue;
                }
            };
            if resp.get("id").and_then(|v| v.as_u64()) == Some(id) {
                return Ok(resp);
            }
            debug!("跳过不匹配的 rdkit helper 响应 id: {:?}", resp.get("id"));
        }
    }

    /// gen3d 便捷方法：`op=gen3d`，成功解析为 [`Gen3dOutput`]。
    ///
    /// 错误码 `RDKIT_INVALID_SMILES` → [`AssembleError::InvalidSmiles`]；
    /// 其它 error → [`AssembleError::Rdkit`]。
    pub async fn gen3d(&mut self, smiles: &str, charge: i8, mult: u8) -> Result<Gen3dOutput> {
        let req = json!({
            "op": "gen3d",
            "smiles": smiles,
            "charge": charge,
            "mult": mult,
        });
        let resp = self.request(&req).await?;

        if resp.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            let result = resp.get("result").ok_or_else(|| AssembleError::Rdkit {
                message: "gen3d 响应缺少 result 字段".to_string(),
            })?;
            let inchikey = result
                .get("inchikey")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let xyz = result
                .get("xyz")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let n_atoms = result.get("n_atoms").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let warnings = result
                .get("warnings")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|w| w.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            return Ok(Gen3dOutput {
                inchikey,
                xyz,
                n_atoms,
                warnings,
            });
        }

        let error = resp.get("error").ok_or_else(|| AssembleError::Rdkit {
            message: "gen3d 失败响应缺少 error 字段".to_string(),
        })?;
        let code = error.get("code").and_then(|v| v.as_str()).unwrap_or("");
        let message = error
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if code == "RDKIT_INVALID_SMILES" {
            return Err(AssembleError::InvalidSmiles {
                smiles: smiles.to_string(),
            });
        }
        Err(AssembleError::Rdkit {
            message: format!("{code}: {message}"),
        })
    }

    /// 看门：子进程已死则重启一次（[`AssembleError::HelperGone`] 自愈）。
    pub async fn ensure_alive(&mut self) -> Result<()> {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                warn!("rdkit helper 已退出（{status:?}），重启");
                self.respawn().await?;
            }
            Ok(None) => {} // 存活
            Err(e) => return Err(AssembleError::Io(e)),
        }
        Ok(())
    }

    /// 重启子进程（复用已探测的 conda / main.py 路径）。
    async fn respawn(&mut self) -> Result<()> {
        // 丢弃旧 stdio（旧进程已死，其 stdout 读取器随之失效）。
        self.stdin = None;
        let (child, stdin, stdout) = spawn_child(&self.conda, &self.main_py).await?;
        self.child = child;
        self.stdin = Some(stdin);
        self.stdout = stdout;
        Ok(())
    }

    /// 优雅关闭：drop stdin → helper 读到 EOF → 自行退出（看门契约）。
    pub async fn shutdown(&mut self) {
        self.stdin = None;
        // 等待其自然退出；若异常则由 kill_on_drop(true) 兜底。
        let _ = self.child.wait().await;
    }
}

/// 默认 helper 入口脚本路径（仓库根 `python/rdkit_helper/main.py`，canonicalize）。
fn default_main_py() -> Result<PathBuf> {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../python/rdkit_helper/main.py")
        .canonicalize()
        .map_err(|e| AssembleError::Rdkit {
            message: format!("rdkit helper main.py 定位失败: {e}"),
        })
}

/// 拉起子进程（数组参数，禁拼接命令）；stderr 泵到 tracing。
async fn spawn_child(
    conda: &Path,
    main_py: &Path,
) -> Result<(Child, ChildStdin, BufReader<ChildStdout>)> {
    let mut cmd = Command::new(conda);
    // --no-capture-output：让子进程 stdout/stderr 直通我们的管道，
    // 否则 conda run 会缓冲捕获 stdout，破坏「一行一响应」协议。
    cmd.args(["run", "--no-capture-output", "-n", "xtbp", "python"])
        .arg(main_py)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| AssembleError::Rdkit {
        message: format!("拉起 rdkit helper 失败（{}）: {e}", main_py.display()),
    })?;

    let stdin = child.stdin.take().ok_or_else(|| AssembleError::Rdkit {
        message: "rdkit helper stdin 未就绪".to_string(),
    })?;
    let stdout = child.stdout.take().ok_or_else(|| AssembleError::Rdkit {
        message: "rdkit helper stdout 未就绪".to_string(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| AssembleError::Rdkit {
        message: "rdkit helper stderr 未就绪".to_string(),
    })?;

    // stderr 泵到 tracing（就绪消息 / RDKit 解析日志都走 stderr）。
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            debug!("rdkit_helper stderr: {line}");
        }
    });

    Ok((child, stdin, BufReader::new(stdout)))
}
