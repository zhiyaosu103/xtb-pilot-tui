//! 领域模型：计算结果（设计文档 §3.4/§3.5）。
//!
//! 只做机械回收：标量结果行式入库；sTDA 跃迁表与展宽谱；
//! 所有结果带 `method_tier: screening` 标记（§4.4 精度声明，不解释数据
//! 但也不隐瞒方法学边界）。

use serde::{Deserialize, Serialize};

/// 方法精度层级标记（§4.4：筛选级方法）。
pub const METHOD_TIER_SCREENING: &str = "screening";

/// 行式标量结果（(job, key, value, unit) 行）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScalarResult {
    /// 结果键（如 "total_energy"、"homo_lumo_gap"）。
    pub key: String,
    /// 数值。
    pub value: f64,
    /// 单位（如 "Eh"、"eV"、"Debye"）。
    pub unit: String,
    /// 方法精度层级。
    pub tier: String,
}

/// 单条电子跃迁（sTDA 跃迁表行）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    /// 态序号（1 起）。
    pub state: u32,
    /// 激发能（eV）。
    pub energy_ev: f64,
    /// 波长（nm）。
    pub wavelength_nm: f64,
    /// 振子强度（长度规范，fL）。
    pub oscillator_strength: f64,
    /// 主要跃迁标记（如 "10→12 / 9→11"，可选）。
    pub assignment: Option<String>,
}

/// 展宽类型。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Broadening {
    /// 高斯展宽（σ，eV）。
    Gaussian { sigma_ev: f64 },
    /// 洛伦兹展宽（γ，eV）。
    Lorentzian { gamma_ev: f64 },
}

/// 展宽后的光谱（(波长 nm, 强度) 采样点序列）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spectrum {
    /// 跃迁表。
    pub transitions: Vec<Transition>,
    /// 展宽参数。
    pub broadening: Broadening,
    /// 采样点（波长 nm 升序，强度为叠加值）。
    pub points: Vec<(f64, f64)>,
}

impl Spectrum {
    /// 由跃迁表机械展宽（高斯/洛伦兹，§3.4 简单后处理）。
    ///
    /// 波长范围：最低跃迁波长 − 40nm 到最高跃迁波长 + 40nm，
    /// 步长 0.5nm；对每个采样点叠加所有跃迁的贡献。
    pub fn broaden(transitions: &[Transition], broadening: Broadening) -> Self {
        const STEP_NM: f64 = 0.5;
        const PAD_NM: f64 = 40.0;
        let mut points = Vec::new();
        if transitions.is_empty() {
            return Self {
                transitions: transitions.to_vec(),
                broadening,
                points,
            };
        }
        let wl_min = transitions
            .iter()
            .map(|t| t.wavelength_nm)
            .fold(f64::INFINITY, f64::min);
        let wl_max = transitions
            .iter()
            .map(|t| t.wavelength_nm)
            .fold(0.0, f64::max);
        let mut nm = (wl_min - PAD_NM).max(1.0);
        let end = wl_max + PAD_NM;
        while nm <= end {
            let mut intensity = 0.0;
            for t in transitions {
                if t.oscillator_strength <= 0.0 {
                    continue;
                }
                // 能量差 → 半宽转换：E = 1239.84 / λ
                const HC: f64 = 1239.84;
                let de = HC * (1.0 / nm - 1.0 / t.wavelength_nm);
                let contrib = match broadening {
                    Broadening::Gaussian { sigma_ev } => {
                        let z = de / sigma_ev;
                        (-0.5 * z * z).exp()
                    }
                    Broadening::Lorentzian { gamma_ev } => {
                        let g2 = gamma_ev * gamma_ev;
                        g2 / (de * de + g2)
                    }
                };
                intensity += t.oscillator_strength * contrib;
            }
            points.push((nm, intensity));
            nm += STEP_NM;
        }
        Self {
            transitions: transitions.to_vec(),
            broadening,
            points,
        }
    }

    /// 导出 (nm, intensity) CSV 行。
    pub fn csv_rows(&self) -> Vec<(f64, f64)> {
        self.points.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaussian_peak_at_transition_wavelength() {
        let transitions = vec![Transition {
            state: 1,
            energy_ev: 4.0,
            wavelength_nm: 300.0,
            oscillator_strength: 1.0,
            assignment: None,
        }];
        let spectrum = Spectrum::broaden(&transitions, Broadening::Gaussian { sigma_ev: 0.1 });
        // 峰值应出现在 300nm（采样步长 0.5，找最近点）
        let (peak_nm, peak_i) = spectrum
            .points
            .iter()
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .unwrap();
        assert!((*peak_nm - 300.0).abs() <= 0.5);
        assert!((*peak_i - 1.0).abs() < 0.01);
    }

    #[test]
    fn lorentzian_peak_value() {
        let transitions = vec![Transition {
            state: 1,
            energy_ev: 4.0,
            wavelength_nm: 300.0,
            oscillator_strength: 1.0,
            assignment: None,
        }];
        let spectrum = Spectrum::broaden(&transitions, Broadening::Lorentzian { gamma_ev: 0.1 });
        let (_, peak_i) = spectrum
            .points
            .iter()
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .unwrap();
        assert!((*peak_i - 1.0).abs() < 0.01);
    }

    #[test]
    fn empty_transitions_yield_empty_points() {
        let spectrum = Spectrum::broaden(&[], Broadening::Gaussian { sigma_ev: 0.1 });
        assert!(spectrum.points.is_empty());
    }

    #[test]
    fn scalar_result_serializes_tier() {
        let r = ScalarResult {
            key: "total_energy".into(),
            value: -15.8796,
            unit: "Eh".into(),
            tier: METHOD_TIER_SCREENING.into(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("screening"));
        let back: ScalarResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }
}
