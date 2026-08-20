//! 协议类型：JSON-RPC 2.0 请求/响应/错误与全部方法参数 schema
//! （设计文档 §3.7）。`schemars` 从 Rust 类型生成 JSON-Schema，
//! `api_schema()` 供 agent 自省。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::future::Future;
use xtbp_core::job::JobParams;

/// 协议版本。
pub const JSONRPC_VERSION: &str = "2.0";

/// 方法名常量（§3.7 方法域，刻意保持小）。
pub mod methods {
    pub const MOL_CREATE: &str = "mol.create";
    pub const MOL_LIST: &str = "mol.list";
    pub const MOL_GET: &str = "mol.get";
    pub const JOB_SUBMIT: &str = "job.submit";
    pub const JOB_STATUS: &str = "job.status";
    pub const JOB_TAIL: &str = "job.tail";
    pub const JOB_CANCEL: &str = "job.cancel";
    pub const JOB_LIST: &str = "job.list";
    pub const WF_RUN: &str = "wf.run";
    pub const WF_STATUS: &str = "wf.status";
    pub const RES_SCALAR: &str = "res.scalar";
    pub const RES_SPECTRUM: &str = "res.spectrum";
    pub const RES_EXPORT: &str = "res.export";
    pub const INST_LIST: &str = "inst.list";
    pub const SYS_HEALTH: &str = "sys.health";
    /// 订阅：job 事件推送。
    pub const JOB_EVENTS: &str = "job.events";
    /// 订阅：队列深度推送。
    pub const QUEUE_EVENTS: &str = "queue.events";
}

/// 客户端 → daemon 请求（一行一个 JSON）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// 固定 "2.0"。
    pub jsonrpc: String,
    /// 请求 id（数字或字符串；通知为 null）。
    #[serde(default)]
    pub id: Option<serde_json::Value>,
    /// 方法名。
    pub method: String,
    /// 参数（缺省 {}）。
    #[serde(default)]
    pub params: serde_json::Value,
    /// 本地 token（§3.7 鉴权一级）。
    #[serde(default)]
    pub token: Option<String>,
}

/// daemon → 客户端响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    /// 固定 "2.0"。
    pub jsonrpc: String,
    /// 与请求一致的 id。
    pub id: Option<serde_json::Value>,
    /// 成功结果。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// 失败错误。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

/// 服务器推送通知帧（订阅事件，无 id）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    /// 固定 "2.0"。
    pub jsonrpc: String,
    /// 通道名（"job.events" / "queue.events"）。
    pub method: String,
    /// 事件负载（JobEvent JSON）。
    pub params: serde_json::Value,
}

impl Response {
    /// 成功响应。
    pub fn ok(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// 错误响应。
    pub fn err(id: Option<serde_json::Value>, error: ApiError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// 结构化错误（JSON-RPC 错误码 + 应用错误码 data.code）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    /// JSON-RPC 错误码：-32700 解析 / -32600 无效请求 / -32601 方法不存在 /
    /// -32602 参数无效 / -32000 应用错误 / -32001 鉴权失败。
    pub code: i64,
    /// 人类可读消息。
    pub message: String,
    /// 结构化负载：`{"code": "XTB_CONVERGENCE_FAILED", ...}`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl ApiError {
    /// 应用错误（携带结构化错误码）。
    pub fn app(structured_code: &str, message: impl Into<String>) -> Self {
        Self {
            code: -32000,
            message: message.into(),
            data: Some(serde_json::json!({ "code": structured_code })),
        }
    }

    /// 方法不存在。
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("方法不存在: {method}"),
            data: None,
        }
    }

    /// 参数无效。
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
            data: None,
        }
    }

    /// 内部错误。
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: message.into(),
            data: None,
        }
    }

    /// 鉴权失败。
    pub fn auth_failed() -> Self {
        Self::app(xtbp_core::error_codes::AUTH_FAILED, "token 无效或缺失")
    }

    /// 取结构化错误码（无则 None）。
    pub fn structured_code(&self) -> Option<&str> {
        self.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(|c| c.as_str())
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

impl std::error::Error for ApiError {}

/// 应用 handler（daemon 实现；与订阅无关的方法全走这里）。
///
/// RPITIT（edition 2024）而非裸 `async fn`：显式声明 Future 为 `Send`，
/// 满足 daemon 里 `tokio::spawn` 的要求。
pub trait ApiHandler: Send + Sync + 'static {
    /// 处理一个方法调用。
    fn handle(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> impl Future<Output = std::result::Result<serde_json::Value, ApiError>> + Send;
}

// ---------------------------------------------------------------------------
// 各方法参数类型（schemars 生成 JSON-Schema）
// ---------------------------------------------------------------------------

/// mol.create 参数。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct MolCreateParams {
    /// SMILES。
    pub smiles: String,
    /// 电荷。
    pub charge: i8,
    /// 多重度。
    pub multiplicity: u8,
    /// 名称（可选）。
    pub name: Option<String>,
}

impl Default for MolCreateParams {
    fn default() -> Self {
        Self {
            smiles: String::new(),
            charge: 0,
            multiplicity: 1,
            name: None,
        }
    }
}

/// mol.list 参数。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct MolListParams {
    /// 条数上限。
    pub limit: i64,
}

impl Default for MolListParams {
    fn default() -> Self {
        Self { limit: 200 }
    }
}

/// mol.get 参数。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MolGetParams {
    /// 分子 id。
    pub molecule_id: String,
}

/// job.submit / wf.run 参数（模板 ID + 参数覆盖）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobSubmitParams {
    /// 已有分子 id（与 smiles 二选一）。
    pub molecule_id: Option<String>,
    /// 或直接给 SMILES（+电荷/多重度）。
    pub smiles: Option<String>,
    /// 电荷（仅 smiles 路径）。
    #[serde(default)]
    pub charge: i8,
    /// 多重度（仅 smiles 路径）。
    #[serde(default)]
    pub multiplicity: u8,
    /// 工作流模板 id（opt / conformer / opt-freq / excited / redox /
    /// reorg-4pt / solv-series）。
    pub workflow: String,
    /// 参数覆盖（合并进模板默认）。
    #[serde(default)]
    pub params: JobParams,
    /// 优先级（0 = 交互，越大越后）。
    #[serde(default)]
    pub priority: u8,
    /// dry-run：只校验并返回组装预览，不入队。
    #[serde(default)]
    pub dry_run: bool,
}

/// job.status / wf.status 参数。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobIdParams {
    /// 任务 id。
    pub job_id: String,
}

/// job.tail 参数。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct JobTailParams {
    /// 任务 id。
    pub job_id: String,
    /// 行偏移（0 起）。
    pub offset: u64,
    /// 本次最多返回行数。
    pub limit: u64,
}

impl Default for JobTailParams {
    fn default() -> Self {
        Self {
            job_id: String::new(),
            offset: 0,
            limit: 100,
        }
    }
}

/// job.list 参数。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct JobListParams {
    /// 状态过滤（如 ["running","queued"]；空 = 全部）。
    pub statuses: Vec<String>,
    /// 按分子过滤。
    pub molecule_id: Option<String>,
    /// 按模板过滤。
    pub workflow: Option<String>,
    /// 条数上限。
    pub limit: i64,
}

impl Default for JobListParams {
    fn default() -> Self {
        Self {
            statuses: vec![],
            molecule_id: None,
            workflow: None,
            limit: 200,
        }
    }
}

/// res.scalar 参数。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResScalarParams {
    /// 任务 id。
    pub job_id: String,
    /// 可选：只取该键。
    pub key: Option<String>,
}

/// res.spectrum 参数。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResSpectrumParams {
    /// 任务 id。
    pub job_id: String,
    /// 光谱类型（缺省取第一类）。
    pub kind: Option<String>,
}

/// res.export 参数。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ResExportParams {
    /// 任务 id（缺省导出全部已完成任务）。
    pub job_id: Option<String>,
    /// 格式：csv / json（zip 在 P5 加入）。
    pub format: String,
    /// 输出目录（daemon 侧路径；缺省 ~/.local/share/xtbpilot/exports）。
    pub out_dir: Option<String>,
}

impl Default for ResExportParams {
    fn default() -> Self {
        Self {
            job_id: None,
            format: "json".into(),
            out_dir: None,
        }
    }
}

/// job.events / queue.events 订阅参数。
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SubscribeParams {
    /// true 订阅 / false 退订。
    pub subscribe: bool,
    /// 可选：只收该任务的事件。
    pub job_id: Option<String>,
}

/// 全部方法参数 schema 的聚合入口。
pub enum Params {}

/// 生成协议 JSON-Schema（供 agent 自省）。
pub fn api_schema() -> serde_json::Value {
    use schemars::schema_for;
    serde_json::json!({
        "protocol": "xtbp-jsonrpc-v1",
        "jsonrpc": JSONRPC_VERSION,
        "framing": "NDJSON：一行一个 JSON 对象（TCP 127.0.0.1:7700 / UDS）",
        "auth": "每个请求携带顶层 token 字段（与 ~/.xtbpilot/agent.json 一致）",
        "events": {
            "job.events": "订阅后服务器推送 JobEvent 通知帧（无 id）",
            "queue.events": "订阅后服务器推送队列深度通知帧（无 id）"
        },
        "methods": {
            methods::MOL_CREATE: schema_for!(MolCreateParams),
            methods::MOL_LIST: schema_for!(MolListParams),
            methods::MOL_GET: schema_for!(MolGetParams),
            methods::JOB_SUBMIT: schema_for!(JobSubmitParams),
            methods::JOB_STATUS: schema_for!(JobIdParams),
            methods::JOB_TAIL: schema_for!(JobTailParams),
            methods::JOB_CANCEL: schema_for!(JobIdParams),
            methods::JOB_LIST: schema_for!(JobListParams),
            methods::WF_RUN: schema_for!(JobSubmitParams),
            methods::WF_STATUS: schema_for!(JobIdParams),
            methods::RES_SCALAR: schema_for!(ResScalarParams),
            methods::RES_SPECTRUM: schema_for!(ResSpectrumParams),
            methods::RES_EXPORT: schema_for!(ResExportParams),
            methods::INST_LIST: schema_for!(serde_json::Value),
            methods::SYS_HEALTH: schema_for!(serde_json::Value),
            methods::JOB_EVENTS: schema_for!(SubscribeParams),
            methods::QUEUE_EVENTS: schema_for!(SubscribeParams),
        },
        "error_codes": [
            xtbp_core::error_codes::XTB_CONVERGENCE_FAILED,
            xtbp_core::error_codes::RESOURCE_EXHAUSTED,
            xtbp_core::error_codes::RDKIT_INVALID_SMILES,
            xtbp_core::error_codes::COMPONENT_UNAVAILABLE,
            xtbp_core::error_codes::CANCELLED,
            xtbp_core::error_codes::TIMEOUT,
            xtbp_core::error_codes::PARSE_DEGRADED,
            xtbp_core::error_codes::JOB_NOT_FOUND,
            xtbp_core::error_codes::AUTH_FAILED,
            xtbp_core::error_codes::INVALID_PARAMS,
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let req = Request {
            jsonrpc: JSONRPC_VERSION.into(),
            id: Some(serde_json::json!(1)),
            method: methods::SYS_HEALTH.into(),
            params: serde_json::json!({}),
            token: Some("t".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(back.method, methods::SYS_HEALTH);
        assert_eq!(back.token.as_deref(), Some("t"));
    }

    #[test]
    fn error_carries_structured_code() {
        let err = ApiError::app(xtbp_core::error_codes::TIMEOUT, "超时");
        assert_eq!(err.structured_code(), Some("TIMEOUT"));
        assert_eq!(err.code, -32000);
    }

    #[test]
    fn schema_lists_all_methods() {
        let schema = api_schema();
        let methods_map = schema["methods"].as_object().unwrap();
        for m in [
            methods::MOL_CREATE,
            methods::JOB_SUBMIT,
            methods::SYS_HEALTH,
            methods::RES_EXPORT,
            methods::JOB_EVENTS,
        ] {
            assert!(methods_map.contains_key(m), "缺少方法 schema: {m}");
        }
    }

    #[test]
    fn submit_params_schema_roundtrip() {
        let p = JobSubmitParams {
            molecule_id: None,
            smiles: Some("C1=CC=CC=C1".into()),
            charge: 0,
            multiplicity: 1,
            workflow: "opt".into(),
            params: JobParams::default(),
            priority: 0,
            dry_run: false,
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["workflow"], "opt");
    }
}
