//! 领域模型：分子（设计文档 §3.1：inchikey 去重）。
//!
//! SMILES 为输入真源；inchikey 由 RDKit helper 计算后回填
//! （未知时为空字符串，组装阶段再填充）。

use crate::id::Ulid;
use serde::{Deserialize, Serialize};

/// 分子电荷（整数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Charge(pub i8);

/// 自旋多重度（≥1；默认单重态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Multiplicity(pub u8);

impl Multiplicity {
    /// 构造并校验多重度 ≥ 1。
    pub fn new(v: u8) -> Result<Self, &'static str> {
        if v == 0 {
            return Err("多重度必须 ≥ 1");
        }
        Ok(Self(v))
    }
}

/// 分子记录（`molecules` 表行）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Molecule {
    /// 主键。
    pub id: Ulid,
    /// InChIKey（首段 14 字符；未知为空串）。
    pub inchikey: String,
    /// 输入 SMILES。
    pub smiles: String,
    /// 电荷。
    pub charge: Charge,
    /// 多重度。
    pub multiplicity: Multiplicity,
    /// 用户可读名称（可选）。
    pub name: Option<String>,
    /// 建库时间（Unix 秒）。
    pub created_at: i64,
}

impl Molecule {
    /// 由 SMILES 构造（id 新生成，inchikey 待回填）。
    pub fn new(
        smiles: impl Into<String>,
        charge: Charge,
        multiplicity: Multiplicity,
        now: i64,
    ) -> Self {
        Self {
            id: Ulid::new(),
            inchikey: String::new(),
            smiles: smiles.into(),
            charge,
            multiplicity,
            name: None,
            created_at: now,
        }
    }

    /// 去重键：inchikey（回填前按 SMILES+电荷+多重度）。
    pub fn dedup_key(&self) -> String {
        if self.inchikey.is_empty() {
            format!("{}|{}|{}", self.smiles, self.charge.0, self.multiplicity.0)
        } else {
            self.inchikey.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiplicity_validates_minimum() {
        assert!(Multiplicity::new(0).is_err());
        assert_eq!(Multiplicity::new(3).unwrap().0, 3);
    }

    #[test]
    fn dedup_key_uses_smiles_before_inchikey() {
        let mol = Molecule::new("C1=CC=CC=C1", Charge(0), Multiplicity(1), 0);
        assert_eq!(mol.dedup_key(), "C1=CC=CC=C1|0|1");
        let mut filled = mol.clone();
        filled.inchikey = "UHOVQNZJYSORNB-UHFFFAOYSA-N".into();
        assert_eq!(filled.dedup_key(), "UHOVQNZJYSORNB-UHFFFAOYSA-N");
    }

    #[test]
    fn molecule_serializes_roundtrip() {
        let mol = Molecule::new("CCO", Charge(0), Multiplicity(1), 1_700_000_000);
        let json = serde_json::to_string(&mol).unwrap();
        let back: Molecule = serde_json::from_str(&json).unwrap();
        assert_eq!(back, mol);
    }
}
