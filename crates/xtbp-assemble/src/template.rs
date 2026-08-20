//! 工作流模板占位符渲染（设计文档 §3.4：模板只声明骨架，渲染在 xtbp-assemble）。
//!
//! 支持的占位符：`{smiles}` `{charge}` `{mult}` `{threads}` `{input_xyz}`
//! `{solvent}` `{method_flags}`。其中 `{method_flags}` 展开为 [`Method::xtb_flags`]
//! 的数组元素，就地展开进 command 数组（仅作为独立 argv 元素时生效）。
//! 未知占位符 / 未闭合占位符 → [`AssembleError::Template`]。

use crate::{AssembleError, Result};
use xtbp_core::method::Method;
use xtbp_core::workflow::WorkflowStep;

/// 渲染上下文（占位符取值来源）。
pub struct RenderCtx<'a> {
    /// 输入 SMILES。
    pub smiles: &'a str,
    /// 电荷。
    pub charge: i8,
    /// 多重度。
    pub mult: u8,
    /// OMP 线程数。
    pub threads: u32,
    /// 计算方法（xtb 系列步骤用，驱动 `{method_flags}`）。
    pub method: &'a Method,
    /// 上游 xyz 文件名（无上游时 "mol.xyz"）。
    pub input_xyz: &'a str,
    /// 可选溶剂名（仅 `{solvent}` 占位符；未设置却引用时视为模板错误）。
    pub solvent: Option<&'a str>,
}

/// 单个步骤的渲染结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOutput {
    /// 渲染后的命令参数数组（`{method_flags}` 已就地展开）。
    pub command: Vec<String>,
    /// 渲染后的输入文件列表。
    pub inputs: Vec<String>,
}

/// 渲染单个步骤：把 command / inputs 中所有 `{placeholder}` 替换。
pub fn render_step(step: &WorkflowStep, ctx: &RenderCtx) -> Result<RenderOutput> {
    let command = render_command(&step.command, ctx)?;
    let inputs = render_list(&step.inputs, ctx)?;
    Ok(RenderOutput { command, inputs })
}

/// 渲染命令数组：`{method_flags}` 作为独立元素时就地展开为多个 argv。
fn render_command(argv: &[String], ctx: &RenderCtx) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for arg in argv {
        if arg == "{method_flags}" {
            out.extend(ctx.method.xtb_flags());
        } else {
            out.push(render_one(arg, ctx)?);
        }
    }
    Ok(out)
}

/// 渲染输入文件列表（纯字符串替换，无数组展开）。
fn render_list(items: &[String], ctx: &RenderCtx) -> Result<Vec<String>> {
    items.iter().map(|s| render_one(s, ctx)).collect()
}

/// 渲染单个字符串中的所有 `{placeholder}`。
fn render_one(s: &str, ctx: &RenderCtx) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            return Err(AssembleError::Template {
                message: format!("未闭合占位符: {s}"),
            });
        };
        let name = &after[..end];
        let replacement = lookup(name, ctx).ok_or_else(|| AssembleError::Template {
            message: format!("未知占位符: {{{name}}}"),
        })?;
        out.push_str(&replacement);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// 按占位符名取值；未知返回 `None`（由调用方转成 [`AssembleError::Template`]）。
fn lookup(name: &str, ctx: &RenderCtx) -> Option<String> {
    match name {
        "smiles" => Some(ctx.smiles.to_string()),
        "charge" => Some(ctx.charge.to_string()),
        "mult" => Some(ctx.mult.to_string()),
        "threads" => Some(ctx.threads.to_string()),
        "input_xyz" => Some(ctx.input_xyz.to_string()),
        "solvent" => ctx.solvent.map(str::to_string),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xtbp_core::method::{MethodFamily, Solvent};

    fn step(command: &[&str], inputs: &[&str]) -> WorkflowStep {
        WorkflowStep {
            id: "opt".into(),
            depends_on: vec![],
            component: "xtb".into(),
            command: command.iter().map(|s| s.to_string()).collect(),
            inputs: inputs.iter().map(|s| s.to_string()).collect(),
            env: Default::default(),
            resources: Default::default(),
            outputs: vec![],
            collect: vec![],
            on_failure: Default::default(),
        }
    }

    fn ctx<'a>(method: &'a Method, solvent: Option<&'a str>) -> RenderCtx<'a> {
        RenderCtx {
            smiles: "CCO",
            charge: 1,
            mult: 2,
            threads: 8,
            method,
            input_xyz: "mol.xyz",
            solvent,
        }
    }

    #[test]
    fn renders_all_scalar_placeholders() {
        let s = step(
            &[
                "xtb",
                "{input_xyz}",
                "--charge",
                "{charge}",
                "--uhf",
                "{mult}",
                "--parallel",
                "{threads}",
            ],
            &["{input_xyz}", "{solvent}.xyz"],
        );
        let out = render_step(&s, &ctx(&Method::gfn2(), Some("water"))).unwrap();
        assert_eq!(
            out.command,
            vec![
                "xtb",
                "mol.xyz",
                "--charge",
                "1",
                "--uhf",
                "2",
                "--parallel",
                "8"
            ]
        );
        assert_eq!(out.inputs, vec!["mol.xyz", "water.xyz"]);
    }

    #[test]
    fn method_flags_expand_inline() {
        let m = Method {
            family: MethodFamily::Gfn2Xtb,
            solvent: Some(Solvent("toluene".into())),
        };
        let s = step(&["xtb", "{input_xyz}", "{method_flags}"], &[]);
        let out = render_step(&s, &ctx(&m, None)).unwrap();
        assert_eq!(
            out.command,
            vec!["xtb", "mol.xyz", "--gfn", "2", "--alpb", "toluene"]
        );
    }

    #[test]
    fn smiles_placeholder_renders() {
        let s = step(&["gen3d", "{smiles}"], &[]);
        let out = render_step(&s, &ctx(&Method::gfn2(), None)).unwrap();
        assert_eq!(out.command, vec!["gen3d", "CCO"]);
    }

    #[test]
    fn unknown_placeholder_is_template_error() {
        let s = step(&["xtb", "{nope}"], &[]);
        let err = render_step(&s, &ctx(&Method::gfn2(), None)).unwrap_err();
        assert!(matches!(err, AssembleError::Template { .. }));
        assert!(err.to_string().contains("{nope}"));
    }

    #[test]
    fn unclosed_placeholder_is_template_error() {
        let s = step(&["xtb", "{input_xyz"], &[]);
        let err = render_step(&s, &ctx(&Method::gfn2(), None)).unwrap_err();
        assert!(err.to_string().contains("未闭合"));
    }

    #[test]
    fn missing_solvent_with_placeholder_is_error() {
        let s = step(&["xtb", "{input_xyz}", "{solvent}"], &[]);
        let err = render_step(&s, &ctx(&Method::gfn2(), None)).unwrap_err();
        assert!(matches!(err, AssembleError::Template { .. }));
    }

    #[test]
    fn method_flags_in_inputs_is_unknown() {
        let s = step(&[], &["{method_flags}"]);
        let err = render_step(&s, &ctx(&Method::gfn2(), None)).unwrap_err();
        assert!(matches!(err, AssembleError::Template { .. }));
    }
}
