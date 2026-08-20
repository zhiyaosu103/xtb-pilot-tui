//! 集成测试：HelperClient（假 helper 常驻进程）与计算目录组装。
//!
//! 依赖本机 conda + python3：测试开头探测 `python3 --version`，不可用则 `return`
//! 跳过（避免在无 python 环境硬失败）。假 helper 为纯标准库（`fake_helper.py`），
//! 由 HelperClient 经 `conda run -n xtbp python` 拉起。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::json;
use xtbp_assemble::{AssembleError, HelperClient, assemble};
use xtbp_core::job::JobParams;
use xtbp_core::method::{Method, MethodFamily, Solvation, Solvent};
use xtbp_core::molecule::{Charge, Molecule, Multiplicity};
use xtbp_core::workflow::WorkflowTemplate;

/// 假 helper 的固定产物（与 `tests/fake_helper.py` 保持一致）。
const FAKE_INCHIKEY: &str = "FAKEINCHIKEY-AAAAAAAAAA-N";

/// `tests/fake_helper.py` 绝对路径。
fn fake_helper_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake_helper.py")
}

/// 探测 python3 可用性（跳过逻辑，规划文档建议的简单方案）。
fn python_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 拉起假 helper；环境不可用时返回 None（测试据此跳过）。
async fn spawn_fake() -> Option<HelperClient> {
    match HelperClient::spawn_with_main_py(&fake_helper_path()).await {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("跳过：无法拉起 fake helper（可能缺 conda）: {e}");
            None
        }
    }
}

#[tokio::test]
async fn gen3d_returns_fake_data() {
    if !python_available() {
        eprintln!("跳过：python3 不可用");
        return;
    }
    let Some(mut client) = spawn_fake().await else {
        return;
    };
    let out = client.gen3d("CCO", 0, 1).await.unwrap();
    assert_eq!(out.inchikey, FAKE_INCHIKEY);
    assert_eq!(out.n_atoms, 3);
    assert!(
        out.xyz.starts_with("3\nfake\n"),
        "xyz 应为假数据块: {}",
        out.xyz
    );
    assert_eq!(out.warnings, vec!["假数据警告".to_string()]);
    client.shutdown().await;
}

#[tokio::test]
async fn gen3d_bad_smiles_maps_to_invalid_smiles() {
    if !python_available() {
        eprintln!("跳过：python3 不可用");
        return;
    }
    let Some(mut client) = spawn_fake().await else {
        return;
    };
    let err = client.gen3d("bad", 0, 1).await.unwrap_err();
    assert!(matches!(err, AssembleError::InvalidSmiles { .. }));
    // InvalidSmiles 回显输入 SMILES。
    assert!(err.to_string().contains("bad"));
    client.shutdown().await;
}

#[tokio::test]
async fn ping_roundtrip_through_protocol() {
    if !python_available() {
        eprintln!("跳过：python3 不可用");
        return;
    }
    let Some(mut client) = spawn_fake().await else {
        return;
    };
    let resp = client.request(&json!({ "op": "ping" })).await.unwrap();
    assert_eq!(resp["ok"], json!(true));
    assert_eq!(resp["result"]["pong"], json!(true));
    assert_eq!(resp["result"]["version"], json!("1"));
    client.shutdown().await;
}

#[tokio::test]
async fn ensure_alive_respawns_after_shutdown() {
    if !python_available() {
        eprintln!("跳过：python3 不可用");
        return;
    }
    let Some(mut client) = spawn_fake().await else {
        return;
    };
    // 首次请求正常。
    let out = client.gen3d("CCO", 0, 1).await.unwrap();
    assert_eq!(out.n_atoms, 3);
    // 优雅关闭（drop stdin → helper 读 EOF 退出）。
    client.shutdown().await;
    // 看门自愈：进程已死 → 重启。
    client.ensure_alive().await.unwrap();
    // 重启后再次请求成功。
    let out2 = client.gen3d("CCO", 0, 1).await.unwrap();
    assert_eq!(out2.inchikey, FAKE_INCHIKEY);
    client.shutdown().await;
}

#[tokio::test]
async fn assemble_builds_self_contained_directory() {
    if !python_available() {
        eprintln!("跳过：python3 不可用");
        return;
    }
    let Some(mut client) = spawn_fake().await else {
        return;
    };

    let root = tempfile::tempdir().unwrap();

    let mol = Molecule::new("CCO", Charge(0), Multiplicity(1), 0);
    let template = WorkflowTemplate::parse(MINIMAL_TOML).unwrap();
    let params = test_params();

    let assembled = assemble(&mut client, &mol, &template, &params, root.path())
        .await
        .unwrap();

    // 目录布局：root/data/jobs/<job_id>/ 下 input/ work/ output/。
    assert!(
        assembled.dir.to_string_lossy().contains("/data/jobs/"),
        "dir 应位于 data/jobs 下: {}",
        assembled.dir.display()
    );
    assert_eq!(
        assembled.dir.file_name().unwrap().to_str().unwrap().len(),
        26
    );
    assert!(assembled.dir.join("work").is_dir());
    assert!(assembled.dir.join("output").is_dir());

    // 四个输入文件均为绝对路径且存在。
    assert_eq!(assembled.input_files.len(), 4);
    for f in &assembled.input_files {
        assert!(f.is_absolute(), "输入文件应为绝对路径: {}", f.display());
        assert!(f.is_file(), "输入文件应已写入: {}", f.display());
    }
    let input_dir = assembled.dir.join("input");

    // mol.xyz：假 helper 固定 xyz。
    let mol_xyz = std::fs::read_to_string(input_dir.join("mol.xyz")).unwrap();
    assert!(mol_xyz.starts_with("3\nfake\n"));
    assert!(mol_xyz.contains("C 0.000000 0.000000 0.000000"));

    // job.toml：可反序列化回 JobParams，且与输入快照一致。
    let job_toml = std::fs::read_to_string(input_dir.join("job.toml")).unwrap();
    let parsed: JobParams = toml::from_str(&job_toml).unwrap();
    assert_eq!(parsed, params);

    // xtb.in：extra["xtb_in"] 字符串内容。
    let xtb_in = std::fs::read_to_string(input_dir.join("xtb.in")).unwrap();
    assert_eq!(xtb_in, "$opt\n  verbose=true");

    // cmd.txt：含注释头（组件版本 + 时间）与渲染后命令。
    let cmd_txt = std::fs::read_to_string(input_dir.join("cmd.txt")).unwrap();
    assert!(cmd_txt.contains("# xtbpilot cmd 快照"));
    assert!(cmd_txt.contains("# 组件: xtb 版本: 6.7.1"));
    assert!(cmd_txt.contains("# 生成时间: "));
    assert!(cmd_txt.contains("xtb mol.xyz --gfn 2 --alpb water --opt tight"));

    // cmd_xtb：首步 xtb 类命令（渲染后 argv 数组）。
    assert_eq!(
        assembled.cmd_xtb,
        vec![
            "xtb", "mol.xyz", "--gfn", "2", "--alpb", "water", "--opt", "tight"
        ]
    );

    client.shutdown().await;
}

/// 极简模板：一个 gen3d 步骤 + 一个 xtb 步骤（直接在测试里 toml 解析）。
const MINIMAL_TOML: &str = r#"
id = "opt"
description = "gen3d → GFN2-xTB opt"

[[steps]]
id = "gen3d"
component = "rdkit"
command = ["gen3d", "{smiles}", "{charge}", "{mult}"]
outputs = ["mol.xyz"]

[[steps]]
id = "opt"
component = "xtb"
depends_on = ["gen3d"]
command = ["xtb", "{input_xyz}", "{method_flags}", "--opt", "tight"]
inputs = ["mol.xyz"]
outputs = ["xtbopt.xyz", "xtb.out"]
"#;

/// 组装测试用的 JobParams 快照（含溶剂 / 组件版本 / xtb_in 扩展参数）。
fn test_params() -> JobParams {
    JobParams {
        method: Method {
            family: MethodFamily::Gfn2Xtb,
            solvation: Solvation::Alpb,
            solvent: Some(Solvent("water".into())),
            etemp: None,
            accuracy: None,
            maxiter: None,
        },
        charge: 0,
        multiplicity: 1,
        threads: 4,
        wall_timeout_secs: 3600,
        stall_timeout_secs: 300,
        max_retries: 2,
        component_versions: BTreeMap::from([("xtb".to_string(), "6.7.1".to_string())]),
        extra: BTreeMap::from([("xtb_in".to_string(), json!("$opt\n  verbose=true"))]),
    }
}
