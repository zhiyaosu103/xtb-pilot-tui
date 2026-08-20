//! 解析器实现（设计文档 §4.2：优先机器可读输出，文本解析仅作回退）。
//!
//! 每个解析器对应一个 `parse` 的解析器键，输入为组件输出的文本内容，
//! 输出统一的 [`Parsed`]。解析失败返回 [`ParseError`]，由上层标记
//! `ParseDegraded` 并保留原始文件——解析失败 ≠ job 失败。

use serde::{Deserialize, Serialize};

use super::{ParseError, Result};
use xtbp_core::result::{METHOD_TIER_SCREENING, ScalarResult, Transition};

/// a.u.（原子单位）偶极矩 → Debye 的换算因子（1 ea₀ = 2.541746473 D）。
const AU_TO_DEBYE: f64 = 2.541_746_473;

/// 判定 sTDA「主要跃迁」的系数阈值：|系数| ≥ 0.5（约 ≥25% 权重）。
const MAJOR_TRANSITION_THRESHOLD: f64 = 0.5;

/// 无量纲量的单位标记（空字符串表示无量纲）。
const UNIT_DIMENSIONLESS: &str = "";

/// 解析产物。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Parsed {
    /// 标量结果（key 为稳定命名，见各解析器文档）。
    pub scalars: Vec<ScalarResult>,
    /// 电子跃迁表（仅 sTDA 才有）。
    pub transitions: Option<Vec<Transition>>,
}

/// 按解析器键解析文本内容。
///
/// 解析器键与产出：
/// - `"xtb-json"`：`xtb --json` 的机器可读 JSON。
/// - `"crest-property"`：`crest_property.json` 构象能量与布居。
/// - `"tda-dat"`：stda 的跃迁表文本。
/// - `"xyz"`：xyz 坐标文件（校验原子数与首行声明一致）。
///
/// 失败时错误消息会带上解析器键（经 [`ParseError::Parser`] 包裹），
/// 便于上层定位退化来源。
pub fn parse(parser: &str, content: &str) -> Result<Parsed> {
    let inner = match parser {
        "xtb-json" => parse_xtb_json(content),
        "crest-property" => parse_crest_property(content),
        "tda-dat" => parse_tda_dat(content),
        "xyz" => parse_xyz(content),
        other => {
            return Err(ParseError::Unexpected {
                what: format!("未知解析器键: {other}"),
            });
        }
    };
    inner.map_err(|e| ParseError::Parser {
        parser: parser.to_string(),
        source: Box::new(e),
    })
}

/// 便捷构造带 `screening` 层级的标量结果。
fn scalar(key: &str, value: f64, unit: &str) -> ScalarResult {
    ScalarResult {
        key: key.to_string(),
        value,
        unit: unit.to_string(),
        tier: METHOD_TIER_SCREENING.to_string(),
    }
}

// ---------------------------------------------------------------------------
// xtb-json：xtb --json 机器可读 JSON（xtb 6.7.1 写 xtbout.json）
// ---------------------------------------------------------------------------

/// 解析 `xtb --json` 输出。
///
/// 键/单位约定（JSON 顶层字段 → 稳定键）：
/// - `"total energy"` → `total_energy` / Eh
/// - `"electronic energy"` → `electronic_energy` / Eh
/// - `"HOMO-LUMO gap / eV"` → `homo_lumo_gap` / eV
/// - `"dipole / a.u."`（矢量）→ `dipole` / Debye（取模并换算 a.u.→Debye）
/// - `"gradient norm"` → `gradient_norm` / a.u.（可选，6.7.1 无此字段）
///
/// 非 JSON 输入返回 `Unexpected{what:"非 xtb --json 输出"}`；JSON 顶层缺字段
/// 返回 [`ParseError::MissingField`]。
fn parse_xtb_json(content: &str) -> Result<Parsed> {
    let value: serde_json::Value =
        serde_json::from_str(content).map_err(|_| ParseError::Unexpected {
            what: "非 xtb --json 输出".to_string(),
        })?;
    let obj = value.as_object().ok_or_else(|| ParseError::Unexpected {
        what: "非 xtb --json 输出（顶层不是对象）".to_string(),
    })?;

    let total_energy = get_f64(obj, "total energy")?;
    let electronic_energy = get_f64(obj, "electronic energy")?;
    let homo_lumo_gap = get_f64(obj, "HOMO-LUMO gap / eV")?;
    let dipole = dipole_debye(obj)?;

    let mut scalars = vec![
        scalar("total_energy", total_energy, "Eh"),
        scalar("electronic_energy", electronic_energy, "Eh"),
        scalar("homo_lumo_gap", homo_lumo_gap, "eV"),
        scalar("dipole", dipole, "Debye"),
    ];
    // gradient_norm 可选（xtb 6.7.1 的 JSON 不含此字段）。
    if let Some(v) = obj.get("gradient norm").and_then(|v| v.as_f64()) {
        scalars.push(scalar("gradient_norm", v, "a.u."));
    }

    Ok(Parsed {
        scalars,
        transitions: None,
    })
}

/// 从 JSON 对象取必需的 f64 字段，缺失则 [`ParseError::MissingField`]。
fn get_f64(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Result<f64> {
    obj.get(key)
        .and_then(|v| v.as_f64())
        .ok_or_else(|| ParseError::MissingField {
            field: key.to_string(),
        })
}

/// 解析 `"dipole / a.u."` 字段并换算为总偶极矩（Debye）。
///
/// 实测 xtb 6.7.1 该字段为 `[x, y, z]` 矢量（a.u.）；为兼容版本漂移，
/// 同时接受标量（视为 a.u. 模长）与 `{"x","y","z"}` 对象。
fn dipole_debye(obj: &serde_json::Map<String, serde_json::Value>) -> Result<f64> {
    const KEY: &str = "dipole / a.u.";
    let value = obj.get(KEY).ok_or_else(|| ParseError::MissingField {
        field: KEY.to_string(),
    })?;

    let mag_au = match value {
        serde_json::Value::Array(comps) => {
            let mut mag2 = 0.0;
            for c in comps {
                let x = c.as_f64().ok_or_else(|| ParseError::Unexpected {
                    what: format!("{KEY} 含非数值分量"),
                })?;
                mag2 += x * x;
            }
            mag2.sqrt()
        }
        serde_json::Value::Number(n) => n.as_f64().ok_or_else(|| ParseError::Unexpected {
            what: format!("{KEY} 数值无法表示为 f64"),
        })?,
        serde_json::Value::Object(m) => {
            let x = m.get("x").and_then(|v| v.as_f64());
            let y = m.get("y").and_then(|v| v.as_f64());
            let z = m.get("z").and_then(|v| v.as_f64());
            match (x, y, z) {
                (Some(x), Some(y), Some(z)) => (x * x + y * y + z * z).sqrt(),
                _ => {
                    return Err(ParseError::Unexpected {
                        what: format!("{KEY} 对象缺少 x/y/z 分量"),
                    });
                }
            }
        }
        _ => {
            return Err(ParseError::Unexpected {
                what: format!("{KEY} 既非矢量也非数值"),
            });
        }
    };

    Ok(mag_au * AU_TO_DEBYE)
}

// ---------------------------------------------------------------------------
// crest-property：crest_property.json 构象能量与布居
// ---------------------------------------------------------------------------

/// 解析 `crest_property.json`。
///
/// 读取 `conformers` 数组，每个元素取 `Etot`（总能量 Eh）与 `pop`（布居），
/// 按能量升序编号为 `conf_energy_{i}` / Eh 与 `conf_population_{i}` / 无量纲，
/// 另输出 `n_conformers` / 无量纲。坏 JSON 走 [`ParseError::Json`]。
fn parse_crest_property(content: &str) -> Result<Parsed> {
    let value: serde_json::Value = serde_json::from_str(content)?;
    let conformers = value
        .get("conformers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ParseError::MissingField {
            field: "conformers".to_string(),
        })?;

    let mut confs: Vec<(f64, f64)> = Vec::with_capacity(conformers.len());
    for (idx, item) in conformers.iter().enumerate() {
        let etot =
            item.get("Etot")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| ParseError::MissingField {
                    field: format!("conformers[{idx}].Etot"),
                })?;
        let pop =
            item.get("pop")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| ParseError::MissingField {
                    field: format!("conformers[{idx}].pop"),
                })?;
        confs.push((etot, pop));
    }
    // 按能量升序编号（稳定性：即使输入乱序也保证 i 语义一致）。
    confs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut scalars = Vec::with_capacity(confs.len() * 2 + 1);
    for (i, (etot, pop)) in confs.iter().enumerate() {
        scalars.push(scalar(&format!("conf_energy_{i}"), *etot, "Eh"));
        scalars.push(scalar(
            &format!("conf_population_{i}"),
            *pop,
            UNIT_DIMENSIONLESS,
        ));
    }
    scalars.push(scalar(
        "n_conformers",
        confs.len() as f64,
        UNIT_DIMENSIONLESS,
    ));

    Ok(Parsed {
        scalars,
        transitions: None,
    })
}

// ---------------------------------------------------------------------------
// tda-dat：stda 跃迁表文本
// ---------------------------------------------------------------------------

/// 解析 stda 跃迁表文本（stdout 的
/// "excitation energies, transition moments and TDA amplitudes" 段）。
///
/// 每行格式：`state  eV  nm  fL  Rv(corr)  系数( 占据-> 空轨道) ...`。
/// 输出 `transitions`（state 从 1 起）与标量 `first_excitation_energy` / eV、
/// `first_wavelength` / nm。
fn parse_tda_dat(content: &str) -> Result<Parsed> {
    let mut transitions: Vec<Transition> = Vec::new();
    for line in content.lines() {
        if let Some(t) = parse_transition_line(line) {
            transitions.push(t);
        }
    }
    if transitions.is_empty() {
        return Err(ParseError::Unexpected {
            what: "未找到跃迁表行".to_string(),
        });
    }

    let first = &transitions[0];
    let scalars = vec![
        scalar("first_excitation_energy", first.energy_ev, "eV"),
        scalar("first_wavelength", first.wavelength_nm, "nm"),
    ];

    Ok(Parsed {
        scalars,
        transitions: Some(transitions),
    })
}

/// 解析单行跃迁表；非跃迁行返回 `None`（绝不 panic）。
fn parse_transition_line(line: &str) -> Option<Transition> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if tokens.len() < 5 {
        return None;
    }
    let state: u32 = tokens[0].parse().ok()?;
    let energy_ev: f64 = tokens[1].parse().ok()?;
    let wavelength_nm: f64 = tokens[2].parse().ok()?;
    let oscillator_strength: f64 = tokens[3].parse().ok()?;
    // tokens[4] 为 Rv(corr)，不作回收。

    // 振幅组每 3 个 token 一组：`系数(` `i->` `a)`。
    let mut assignment_parts: Vec<String> = Vec::new();
    let mut i = 5;
    while i + 2 < tokens.len() {
        let coeff: f64 = match tokens[i].trim_end_matches('(').parse() {
            Ok(c) => c,
            Err(_) => {
                i += 1;
                continue;
            }
        };
        let occ = tokens[i + 1].trim_end_matches("->");
        let virt = tokens[i + 2].trim_end_matches(')');
        if occ.chars().all(|c| c.is_ascii_digit()) && virt.chars().all(|c| c.is_ascii_digit()) {
            if coeff.abs() >= MAJOR_TRANSITION_THRESHOLD {
                assignment_parts.push(format!("{occ}→{virt}"));
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    let assignment = if assignment_parts.is_empty() {
        None
    } else {
        Some(assignment_parts.join(" / "))
    };

    Some(Transition {
        state,
        energy_ev,
        wavelength_nm,
        oscillator_strength,
        assignment,
    })
}

// ---------------------------------------------------------------------------
// xyz：坐标文件（两行头 + 元素/坐标行）
// ---------------------------------------------------------------------------

/// 解析 xyz 坐标文本。
///
/// 输出 `n_atoms` / 无量纲，并校验坐标行数与首行声明一致（不一致 →
/// [`ParseError::Unexpected`]）。
fn parse_xyz(content: &str) -> Result<Parsed> {
    let mut lines = content.lines().filter(|l| !l.trim().is_empty());
    let head = lines.next().ok_or_else(|| ParseError::Unexpected {
        what: "空输入".to_string(),
    })?;
    let n_atoms: usize = head.trim().parse().map_err(|_| ParseError::Unexpected {
        what: format!("首行不是原子数: {head}"),
    })?;

    // 第二行为注释行，跳过。
    lines.next();

    let mut count = 0usize;
    for line in lines {
        let t: Vec<&str> = line.split_whitespace().collect();
        // 坐标行：元素符号 + 3 个数值。
        if t.len() >= 4 && t[1..4].iter().all(|s| s.parse::<f64>().is_ok()) {
            count += 1;
        }
    }

    if count != n_atoms {
        return Err(ParseError::Unexpected {
            what: format!("原子数与首行声明不一致: 声明 {n_atoms}，实际 {count}"),
        });
    }

    Ok(Parsed {
        scalars: vec![scalar("n_atoms", n_atoms as f64, UNIT_DIMENSIONLESS)],
        transitions: None,
    })
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_json_is_json_error() {
        // crest-property 输入必须是 JSON：坏 JSON 走 Json 错误。
        let err = parse_crest_property("{ 这不是合法 JSON");
        assert!(matches!(err, Err(ParseError::Json(_))));
    }

    #[test]
    fn missing_field_is_missing_field() {
        // 空对象缺全部顶层字段 → MissingField。
        let err = parse_xtb_json("{}");
        assert!(matches!(err, Err(ParseError::MissingField { .. })));
    }

    #[test]
    fn non_json_text_is_unexpected() {
        // 组件不支持 --json 时 stdout 为人类可读文本 → Unexpected。
        let err = parse_xtb_json("这是 xtb 的人类可读输出，不是 JSON");
        assert!(matches!(err, Err(ParseError::Unexpected { .. })));
    }

    #[test]
    fn unknown_parser_key_is_unexpected() {
        let err = parse("no-such-parser", "任意内容");
        assert!(matches!(err, Err(ParseError::Unexpected { .. })));
    }

    #[test]
    fn parse_error_includes_parser_key() {
        let err = parse("xtb-json", "不是 JSON").unwrap_err();
        assert!(err.to_string().contains("xtb-json"));
    }

    #[test]
    fn tda_transition_line_parses_assignment() {
        let line = "    1    5.181   239.3     0.0000     0.0000    \
                    -0.71(  10->  12)  0.71(   9->  11) -0.00(  10->  13)";
        let t = parse_transition_line(line).unwrap();
        assert_eq!(t.state, 1);
        assert!((t.energy_ev - 5.181).abs() < 1e-9);
        assert!((t.wavelength_nm - 239.3).abs() < 1e-9);
        assert_eq!(t.oscillator_strength, 0.0);
        assert_eq!(t.assignment.as_deref(), Some("10→12 / 9→11"));
    }

    #[test]
    fn tda_header_line_is_not_a_transition() {
        assert!(parse_transition_line(" state    eV      nm       fL").is_none());
        assert!(parse_transition_line("").is_none());
    }

    #[test]
    fn xyz_atom_count_mismatch_is_unexpected() {
        // 声明 3 原子，但只有 2 行坐标。
        let content = "3\nwater\nO 0.0 0.0 0.0\nH 1.0 1.0 1.0\n";
        assert!(matches!(
            parse_xyz(content),
            Err(ParseError::Unexpected { .. })
        ));
    }

    #[test]
    fn xyz_parses_atom_count() {
        let content = "3\nwater\nO 0.0 0.0 0.0\nH 1.0 1.0 1.0\nH 0.0 1.0 0.0\n";
        let parsed = parse_xyz(content).unwrap();
        assert_eq!(parsed.scalars.len(), 1);
        assert_eq!(parsed.scalars[0].key, "n_atoms");
        assert_eq!(parsed.scalars[0].value, 3.0);
    }

    #[test]
    fn xtb_json_dipole_vector_converted_to_debye() {
        let content = r#"{
            "total energy": -5.07020801,
            "HOMO-LUMO gap / eV": 14.15968868,
            "electronic energy": -5.10166616,
            "dipole / a.u.": [0.0, 1.0, 0.0]
        }"#;
        let parsed = parse_xtb_json(content).unwrap();
        let dipole = parsed.scalars.iter().find(|s| s.key == "dipole").unwrap();
        assert!((dipole.value - AU_TO_DEBYE).abs() < 1e-9);
        assert_eq!(dipole.unit, "Debye");
    }
}
