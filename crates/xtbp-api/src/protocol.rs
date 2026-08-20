//! 协议类型：请求 / 响应（一行一个 JSON）。
//!
//! `schemars` 从 Rust 类型生成 JSON-Schema（`api-schema` 命令，规划文档 §2.2）。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 客户端 → daemon 的请求。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "method", content = "params")]
pub enum Request {
    /// 提交一个计算任务（xtb/crest/sTDA 等，payload 由 daemon 解释）。
    Submit {
        task: String,
        payload: serde_json::Value,
    },
    /// 查询任务状态。
    Status { job_id: String },
    /// 健康检查。
    Ping,
}

/// daemon → 客户端的响应。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", content = "payload")]
pub enum Response {
    /// 成功。
    Ok {
        job_id: String,
        data: serde_json::Value,
    },
    /// 失败。
    Err { message: String },
}

/// 生成协议 JSON-Schema（供 `api-schema` 命令输出）。
pub fn api_schema() -> serde_json::Value {
    let request = schemars::schema_for!(Request);
    let response = schemars::schema_for!(Response);
    serde_json::json!({
        "protocol": "xtbp-ndjson-v1",
        "framing": "one JSON object per line (NDJSON over TCP/UDS)",
        "request": request,
        "response": response,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serde_tagged() {
        let req = Request::Ping;
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"method\":\"Ping\""));
        let back: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Request::Ping));
    }

    #[test]
    fn schema_generates_valid_json() {
        let schema = api_schema();
        assert_eq!(schema["protocol"], "xtbp-ndjson-v1");
        assert!(schema["request"].is_object());
    }
}
