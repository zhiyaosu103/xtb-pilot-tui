//! 组件版本（语义化版本，规划文档 §2.2：`semver` 用于组件版本解析与比较）。

use semver::Version;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// 组件语义化版本。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ComponentVersion(Version);

impl ComponentVersion {
    /// 从 `semver::Version` 构造。
    pub fn new(v: Version) -> Self {
        Self(v)
    }

    /// 未知版本占位（0.0.0，如版本探测失败的组件）。
    pub fn zero() -> Self {
        Self(Version::new(0, 0, 0))
    }
}

impl FromStr for ComponentVersion {
    type Err = semver::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self(Version::parse(s)?))
    }
}

impl fmt::Display for ComponentVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_ordering() {
        let a: ComponentVersion = "6.7.1".parse().unwrap();
        let b: ComponentVersion = "6.7.2".parse().unwrap();
        assert!(b > a);
        assert_eq!(a.to_string(), "6.7.1");
    }
}
