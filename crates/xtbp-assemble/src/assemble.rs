//! 计算目录生成（设计文档 §3.1：SMILES → 自洽计算目录，目录即真相）。
//!
//! 产物目录：`<root>/data/jobs/<job_id>/`，含：
//! - `input/mol.xyz`（RDKit 生成的 3D 构型）；
//! - `input/job.toml`（[`JobParams`] 快照，`toml::to_string_pretty`）；
//! - `input/xtb.in`（`JobParams.extra["xtb_in"]` 字符串，否则空文件）；
//! - `input/cmd.txt`（首步计算命令快照，含组件版本与时间，可人工重跑）；
//! - `work/`（运行目录）与 `output/`（产物目录）。
//!
//! 即使 daemon 宕机，人也可以 `cd` 进目录手动重跑 `cmd.txt`。

use crate::helper::HelperClient;
use crate::template::{RenderCtx, render_step};
use crate::{AssembleError, Result};
use std::path::{Path, PathBuf};
use xtbp_core::time::{format_unix, now_unix};
use xtbp_core::workflow::{WorkflowStep, WorkflowTemplate};
use xtbp_core::{JobParams, Molecule, Ulid};

/// 组装产物。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assembled {
    /// 计算目录绝对路径：`.../data/jobs/<job_id>`（末段即新生成的 job id）。
    pub dir: PathBuf,
    /// 生成的输入文件绝对路径（mol.xyz / job.toml / xtb.in / cmd.txt）。
    pub input_files: Vec<PathBuf>,
    /// 首步 xtb 类命令（渲染后 argv 数组，示例用途；无计算步骤时为空）。
    pub cmd_xtb: Vec<String>,
}

/// 组装：gen3d 拿 xyz → 建 `input/ work/ output/` → 写 mol.xyz、job.toml、
/// xtb.in、cmd.txt。
///
/// `root` 为文件仓根（`data/jobs` 挂在 `root/data/jobs`）。
pub async fn assemble(
    helper: &mut HelperClient,
    mol: &Molecule,
    template: &WorkflowTemplate,
    params: &JobParams,
    root: &Path,
) -> Result<Assembled> {
    // 1. 生成 3D 构型（含 InChIKey 去重键）。
    let out = helper
        .gen3d(&mol.smiles, mol.charge.0, mol.multiplicity.0)
        .await?;

    // 2. 计算目录：root/data/jobs/<job_id>（job id 现铸，末段即 id）。
    let job_id = Ulid::new();
    let dir = root.join("data").join("jobs").join(job_id.to_string());
    let input_dir = dir.join("input");
    std::fs::create_dir_all(&input_dir)?;
    std::fs::create_dir_all(dir.join("work"))?;
    std::fs::create_dir_all(dir.join("output"))?;
    // 转绝对路径（目录已存在，canonicalize 成功）。
    let dir = dir.canonicalize()?;
    let input_dir = dir.join("input");

    // 3. mol.xyz（utf-8）。
    let mol_xyz = input_dir.join("mol.xyz");
    std::fs::write(&mol_xyz, out.xyz.as_bytes())?;

    // 4. job.toml：JobParams 快照。
    let job_toml = input_dir.join("job.toml");
    let toml_text = toml::to_string_pretty(params).map_err(|e| AssembleError::Serialize {
        message: format!("job.toml 序列化失败: {e}"),
    })?;
    std::fs::write(&job_toml, toml_text.as_bytes())?;

    // 5. xtb.in：extra["xtb_in"] 为字符串则写入，否则空文件。
    let xtb_in = input_dir.join("xtb.in");
    let xtb_in_text = params
        .extra
        .get("xtb_in")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    std::fs::write(&xtb_in, xtb_in_text.as_bytes())?;

    // 6. cmd.txt：首步计算命令（component != "rdkit"）渲染后写入，注释头含组件版本与时间。
    let cmd_txt = input_dir.join("cmd.txt");
    let cmd_xtb = render_first_command(template, params, mol)?;
    let cmd_txt_body = build_cmd_snapshot(template, params, &cmd_xtb);
    std::fs::write(&cmd_txt, cmd_txt_body.as_bytes())?;

    Ok(Assembled {
        dir,
        input_files: vec![mol_xyz, job_toml, xtb_in, cmd_txt],
        cmd_xtb,
    })
}

/// 渲染首步「计算」命令（拓扑序第一个 component != "rdkit" 的步骤）。
fn render_first_command(
    template: &WorkflowTemplate,
    params: &JobParams,
    mol: &Molecule,
) -> Result<Vec<String>> {
    let Some(step) = first_compute_step(template) else {
        return Ok(Vec::new());
    };
    let ctx = RenderCtx {
        smiles: &mol.smiles,
        charge: params.charge,
        mult: params.multiplicity,
        threads: params.threads,
        method: &params.method,
        input_xyz: "mol.xyz",
        solvent: params.method.solvent.as_ref().map(|s| s.0.as_str()),
    };
    Ok(render_step(step, &ctx)?.command)
}

/// 拓扑序第一个非 rdkit（计算）步骤。
fn first_compute_step(template: &WorkflowTemplate) -> Option<&WorkflowStep> {
    let order = template.topo_order().ok()?;
    order.into_iter().find_map(|i| {
        let step = &template.steps[i];
        (step.component != "rdkit").then_some(step)
    })
}

/// 组装 cmd.txt 内容：注释头（组件版本 + 时间）+ 渲染后命令行。
fn build_cmd_snapshot(
    template: &WorkflowTemplate,
    params: &JobParams,
    cmd_xtb: &[String],
) -> String {
    let mut header = String::from("# xtbpilot cmd 快照\n");
    if let Some(step) = first_compute_step(template) {
        let version = params
            .component_versions
            .get(&step.component)
            .map(String::as_str)
            .unwrap_or("未知");
        header.push_str(&format!("# 组件: {} 版本: {}\n", step.component, version));
    }
    header.push_str(&format!("# 生成时间: {}\n", format_unix(now_unix())));
    let command_line = shell_join(cmd_xtb);
    format!("{header}{command_line}\n")
}

/// argv → 单行命令（含空格时加双引号转义；快照用途，非执行）。
fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 单元素转义：含空白 / 双引号 / 反斜杠时加双引号并转义内部引号。
fn shell_quote(arg: &str) -> String {
    if arg
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\\')
    {
        let escaped = arg.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        arg.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_wraps_spaces_and_escapes_quotes() {
        assert_eq!(shell_quote("simple"), "simple");
        assert_eq!(shell_quote("has space"), "\"has space\"");
        assert_eq!(shell_quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(shell_quote("back\\slash"), "\"back\\\\slash\"");
    }

    #[test]
    fn shell_join_separates_with_spaces() {
        let argv = vec!["xtb".to_string(), "a b".to_string(), "--opt".to_string()];
        assert_eq!(shell_join(&argv), "xtb \"a b\" --opt");
    }
}
