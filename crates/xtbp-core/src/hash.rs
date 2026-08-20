//! 内容哈希（sha256，规划文档 §2.2：内容哈希 / 文件 sha256）。

use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// 计算文件的 sha256 十六进制摘要。
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 计算字节切片的 sha256 十六进制摘要。
pub fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// 内容哈希（设计文档 §3.1：幂等去重键）。
///
/// 对 JSON 值做键排序后的规范序列化再 sha256——同一
/// (inchikey, 方法, 参数, 组件版本) 组合哈希一致，重复提交命中缓存。
pub fn content_hash_json(value: &serde_json::Value) -> String {
    let canonical = canonicalize(value);
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    sha256_bytes(&bytes)
}

/// 递归排序对象键（规范形式）。
fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sorted: std::collections::BTreeMap<_, _> = map
                .iter()
                .map(|(k, v)| (k.clone(), canonicalize(v)))
                .collect();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalize).collect())
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        // "abc" 的 sha256 标准测试向量
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn content_hash_is_key_order_insensitive() {
        let a: serde_json::Value = serde_json::json!({"b": 1, "a": {"y": 2, "x": 3}});
        let b: serde_json::Value = serde_json::json!({"a": {"x": 3, "y": 2}, "b": 1});
        assert_eq!(content_hash_json(&a), content_hash_json(&b));
    }

    #[test]
    fn content_hash_differs_on_value_change() {
        let a = serde_json::json!({"method": "gfn2", "charge": 0});
        let b = serde_json::json!({"method": "gfn2", "charge": 1});
        assert_ne!(content_hash_json(&a), content_hash_json(&b));
    }
}
