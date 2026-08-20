//! fixtures 集成测试：读真实组件输出，断言解析成功与关键数值，
//! 并对 `Parsed` 的稳定字段（scalars + transitions）做 insta JSON 快照。
//!
//! fixtures 生成命令与版本见 `tests/fixtures/README.md`。

use xtbp_parse::{Parsed, parse};

/// 读 fixture 文件内容。
fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读取 fixture {path} 失败: {e}"))
}

/// 按 key 取标量值。
fn scalar_value(parsed: &Parsed, key: &str) -> f64 {
    parsed
        .scalars
        .iter()
        .find(|s| s.key == key)
        .unwrap_or_else(|| panic!("缺少标量 {key}"))
        .value
}

#[test]
fn water_xtb_json_parses() {
    let parsed = parse("xtb-json", &fixture("water_xtb_json.out")).expect("解析失败");

    let total = scalar_value(&parsed, "total_energy");
    assert!((total - (-5.070_208_01)).abs() < 1e-8, "total={total}");
    assert!(
        total < -5.07 && total > -5.08,
        "水 GFN2 总能量应约 -5.07 Eh"
    );

    let gap = scalar_value(&parsed, "homo_lumo_gap");
    assert!((gap - 14.159_688_68).abs() < 1e-8);

    let dipole = scalar_value(&parsed, "dipole");
    assert!(
        (dipole - 2.287).abs() < 0.01,
        "偶极应约 2.287 Debye, 实际 {dipole}"
    );

    assert!(parsed.transitions.is_none(), "单点无跃迁表");

    insta::assert_snapshot!("water_xtb_json", snapshot_json(&parsed));
}

#[test]
fn ethanol_crest_property_parses() {
    let parsed =
        parse("crest-property", &fixture("ethanol_crest_property.json")).expect("解析失败");

    let n = scalar_value(&parsed, "n_conformers");
    assert!(n >= 2.0, "构象数应 ≥ 2, 实际 {n}");
    assert_eq!(n, 3.0);

    let e0 = scalar_value(&parsed, "conf_energy_0");
    assert!((e0 - (-11.394_339_37)).abs() < 1e-6);

    let p0 = scalar_value(&parsed, "conf_population_0");
    assert!((p0 - 0.9625).abs() < 1e-4);

    // 布居总和应接近 1。
    let sum: f64 = (0..n as usize)
        .map(|i| scalar_value(&parsed, &format!("conf_population_{i}")))
        .sum();
    assert!((sum - 1.0).abs() < 1e-2, "布居总和应≈1, 实际 {sum}");

    insta::assert_snapshot!("ethanol_crest_property", snapshot_json(&parsed));
}

#[test]
fn benzene_tda_dat_parses() {
    let parsed = parse("tda-dat", &fixture("benzene_tda.dat")).expect("解析失败");

    let transitions = parsed.transitions.as_ref().expect("应有跃迁表");
    assert_eq!(transitions.len(), 3);

    let first = &transitions[0];
    assert_eq!(first.state, 1);
    assert!((first.energy_ev - 5.181).abs() < 1e-3, "第一激发态 eV≈5.18");
    assert!((first.wavelength_nm - 239.3).abs() < 0.1);
    assert_eq!(
        first.oscillator_strength, 0.0,
        "苯第一激发态为禁阻跃迁 fL==0"
    );
    assert_eq!(first.assignment.as_deref(), Some("10→12 / 9→11"));

    assert!((scalar_value(&parsed, "first_excitation_energy") - 5.181).abs() < 1e-3);
    assert!((scalar_value(&parsed, "first_wavelength") - 239.3).abs() < 0.1);

    insta::assert_snapshot!("benzene_tda_dat", snapshot_json(&parsed));
}

#[test]
fn water_xyz_parses() {
    let parsed = parse("xyz", &fixture("water.xyz")).expect("解析失败");

    assert_eq!(scalar_value(&parsed, "n_atoms"), 3.0);
    assert!(parsed.transitions.is_none());

    insta::assert_snapshot!("water_xyz", snapshot_json(&parsed));
}

/// 序列化 Parsed 稳定字段为 JSON（快照用）。
fn snapshot_json(parsed: &Parsed) -> String {
    serde_json::to_string_pretty(parsed).expect("序列化 Parsed 失败")
}
