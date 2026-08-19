//! 基于 ULID 的可排序 ID（job / run / molecule，规划文档 §2.2：比 uuid 友好）。

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// ULID 新类型：可排序、时间有序、人类可读。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Ulid(ulid::Ulid);

impl Ulid {
    /// 生成一个新的 ULID。
    pub fn new() -> Self {
        Self(ulid::Ulid::new())
    }
}

impl Default for Ulid {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for Ulid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for Ulid {
    type Err = ulid::DecodeError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self(ulid::Ulid::from_str(s)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulid_is_sortable_and_roundtrips() {
        let a = Ulid::new();
        let s = a.to_string();
        assert_eq!(s.parse::<Ulid>().unwrap(), a);
        assert_eq!(s.len(), 26);
        // 排序性用规范示例 ULID（同前缀、末位递增）确定性验证：
        // 同一毫秒内随机生成的 ULID 不保证单调，不能用于排序断言。
        let low: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
        let high: Ulid = "01ARZ3NDEKTSV4RRFFQ69G5FAW".parse().unwrap();
        assert!(high > low);
    }
}
