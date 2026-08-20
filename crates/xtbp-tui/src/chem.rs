//! 化学数据辅助：xyz 解析、Braille 点云投影、收敛曲线提取、外部查看器。

/// 原子（元素 + 三维坐标，Å）。
#[derive(Debug, Clone, PartialEq)]
pub struct Atom {
    pub element: String,
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// 解析 xyz 文本（两行头 + 元素/坐标行）。
pub fn parse_xyz(text: &str) -> Result<Vec<Atom>, String> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next().ok_or("xyz 缺原子数行")?;
    let n: usize = header
        .trim()
        .parse()
        .map_err(|e| format!("xyz 原子数解析失败: {e}"))?;
    let mut atoms = Vec::with_capacity(n);
    for line in lines {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 {
            continue; // 注释行跳过
        }
        atoms.push(Atom {
            element: parts[0].to_string(),
            x: parts[1].parse().map_err(|e| format!("坐标解析失败: {e}"))?,
            y: parts[2].parse().map_err(|e| format!("坐标解析失败: {e}"))?,
            z: parts[3].parse().map_err(|e| format!("坐标解析失败: {e}"))?,
        });
        if atoms.len() >= n {
            break;
        }
    }
    if atoms.len() != n {
        return Err(format!("xyz 原子数不符: 声明 {n}，实际 {}", atoms.len()));
    }
    Ok(atoms)
}

/// 旋转 + 正交投影到 2D（Braille 点云用）。
/// `angle` 为绕 y 轴角度（弧度）；返回 (x, y) 归一化坐标（中心 0,0）。
pub fn project(atoms: &[Atom], angle: f64) -> Vec<(f64, f64, &str)> {
    let (sin, cos) = angle.sin_cos();
    atoms
        .iter()
        .map(|a| {
            let x = a.x * cos - a.z * sin;
            (x, a.y, a.element.as_str())
        })
        .collect()
}

/// 把投影坐标缩放到 canvas 坐标空间（含 10% 边距）。
pub fn fit_to_canvas<'a>(
    pts: &'a [(f64, f64, &'a str)],
    x_min: f64,
    x_max: f64,
    y_min: f64,
    y_max: f64,
) -> Vec<(f64, f64, &'a str)> {
    if pts.is_empty() {
        return Vec::new();
    }
    let (mut px_min, mut px_max) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut py_min, mut py_max) = (f64::INFINITY, f64::NEG_INFINITY);
    for (x, y, _) in pts {
        px_min = px_min.min(*x);
        px_max = px_max.max(*x);
        py_min = py_min.min(*y);
        py_max = py_max.max(*y);
    }
    let pad = 0.08_f64
        .max((px_max - px_min).abs() * 0.08)
        .max((py_max - py_min).abs() * 0.08);
    let (lo_x, hi_x) = (px_min - pad, px_max + pad);
    let (lo_y, hi_y) = (py_min - pad, py_max + pad);
    let w = (hi_x - lo_x).max(1e-9);
    let h = (hi_y - lo_y).max(1e-9);
    pts.iter()
        .map(|(x, y, e)| {
            let cx = x_min + (x - lo_x) / w * (x_max - x_min);
            let cy = y_min + (y - lo_y) / h * (y_max - y_min);
            (cx, cy, *e)
        })
        .collect()
}

/// 元素 → 终端颜色（预览着色）。
pub fn element_color(e: &str) -> ratatui::style::Color {
    match e {
        "H" => ratatui::style::Color::White,
        "C" => ratatui::style::Color::Gray,
        "N" => ratatui::style::Color::LightBlue,
        "O" => ratatui::style::Color::LightRed,
        "S" => ratatui::style::Color::Yellow,
        "F" | "Cl" | "Br" | "I" => ratatui::style::Color::Green,
        _ => ratatui::style::Color::Magenta,
    }
}

/// 从输出行流提取能量序列（收敛曲线）：找含 "energy" 的行，取其后第一个浮点数。
pub fn extract_energies(lines: &[(u64, String)]) -> Vec<f64> {
    lines
        .iter()
        .filter_map(|(_, line)| {
            let lower = line.to_ascii_lowercase();
            let idx = lower.find("energy")?;
            let rest = &line[idx + "energy".len()..];
            // 跳过 "="、":" 与空格后取浮点
            let mut digits = String::new();
            let mut seen_digit = false;
            for ch in rest.chars() {
                if ch.is_ascii_digit()
                    || ch == '.'
                    || ch == '-'
                    || ch == '+'
                    || ch == 'e'
                    || ch == 'E'
                {
                    if ch.is_ascii_digit() {
                        seen_digit = true;
                    }
                    digits.push(ch);
                } else if seen_digit {
                    break;
                }
            }
            digits.parse::<f64>().ok()
        })
        .collect()
}

/// 唤起 Windows 侧查看器（设计文档 §3.6：`o` 键，经 interop cmd.exe）。
/// `wslpath -w` 转 Windows 路径后 `cmd.exe /c start`。
pub fn open_external_viewer(path: &str) -> Result<(), String> {
    let win_path = std::process::Command::new("wslpath")
        .arg("-w")
        .arg(path)
        .output()
        .map_err(|e| format!("wslpath 失败: {e}"))?;
    let win_path = String::from_utf8_lossy(&win_path.stdout).trim().to_string();
    let mut cmd = std::process::Command::new("/mnt/c/Windows/System32/cmd.exe");
    cmd.arg("/c").arg("start").arg("").arg(&win_path);
    cmd.spawn().map_err(|e| format!("启动查看器失败: {e}"))?;
    Ok(())
}

/// 解析 .smi 批量输入文件（SMILES 一行一条，可带名称/注释）：
/// `SMILES [name]`；`#` 开头或空行跳过；行内首个空白后的文本为名称。
pub fn parse_smi(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        let Some(smiles) = it.next() else { continue };
        // 基础形态校验：SMILES 至少含一个字母元素符号（拒绝纯数字/符号行）
        if !smiles.chars().any(|c| c.is_ascii_alphabetic()) {
            continue;
        }
        let name = it.next().unwrap_or("").to_string();
        out.push((smiles.to_string(), name));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const WATER_XYZ: &str = "3\nwater\nO  0.0000  0.0000  0.1173\nH  0.0000  0.7572 -0.4692\nH  0.0000 -0.7572 -0.4692\n";

    #[test]
    fn parse_xyz_basic() {
        let atoms = parse_xyz(WATER_XYZ).unwrap();
        assert_eq!(atoms.len(), 3);
        assert_eq!(atoms[0].element, "O");
        assert!((atoms[0].z - 0.1173).abs() < 1e-9);
    }

    #[test]
    fn parse_xyz_count_mismatch_rejected() {
        let bad = "3\nx\nO 0 0 0\n";
        assert!(parse_xyz(bad).is_err());
    }

    #[test]
    fn project_rotates_and_fits() {
        let atoms = parse_xyz(WATER_XYZ).unwrap();
        let pts = project(&atoms, std::f64::consts::PI / 2.0);
        assert_eq!(pts.len(), 3);
        let fitted = fit_to_canvas(&pts, 0.0, 100.0, 0.0, 40.0);
        assert!(
            fitted
                .iter()
                .all(|(x, y, _)| (0.0..=100.0).contains(x) && (0.0..=40.0).contains(y))
        );
    }

    #[test]
    fn parse_smi_basic_and_skip_rules() {
        let text = "# comment\nc1ccccc1 benzene\nCCO ethanol extra-tokens\n\nC1CCCCC1\n\n";
        let rows = parse_smi(text);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], ("c1ccccc1".to_string(), "benzene".to_string()));
        assert_eq!(rows[1], ("CCO".to_string(), "ethanol".to_string()));
        assert_eq!(rows[2].0, "C1CCCCC1");
    }

    #[test]
    fn parse_smi_rejects_garbage_rows() {
        let text = "123456\n###\n   \nCC";
        let rows = parse_smi(text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "CC");
    }

    #[test]
    fn extract_energies_finds_scf_values() {
        let lines = vec![
            (0, "cycle = 1 SCF energy = -5.070123".to_string()),
            (1, "energy: -15.8796 Eh".to_string()),
            (2, "no number here".to_string()),
        ];
        let energies = extract_energies(&lines);
        assert_eq!(energies.len(), 2);
        assert!((energies[0] + 5.070123).abs() < 1e-9);
    }
}
