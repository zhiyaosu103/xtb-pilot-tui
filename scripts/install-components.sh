#!/usr/bin/env bash
# xTB-Pilot 组件安装：xtb4stda / stda 二进制 + sTDA 参数文件。
# conda-forge 无此二包，从 grimme-lab GitHub Releases 下载预编译二进制，
# 按 InstanceRegistry 约定目录落位（~/opt/<name>-*/bin/<name>），
# 参数文件放入 XTB4STDAHOME 约定目录（daemon 启动时自动探测并启用）。
# 幂等：已存在的文件跳过，可重复执行。
set -euo pipefail

OPT_ROOT="${1:-$HOME/opt}"
XTB4STDA_HOME="$OPT_ROOT/xtb4stda-1.0"
STDA_HOME="$OPT_ROOT/stda-1.6.1"
BASE_URL="https://github.com/grimme-lab/xtb4stda/releases/download/v1.0"
PARAM_URL="https://github.com/grimme-lab/xtb4stda/raw/master"

if ! command -v curl >/dev/null 2>&1; then
    echo "错误：需要 curl（apt install curl / dnf install curl）" >&2
    exit 1
fi

fetch() { # fetch <url> <dest> <chmod?>
    local url="$1" dest="$2"
    echo "==> 下载 ${url##*/}"
    curl -fL --retry 3 --connect-timeout 15 -o "$dest" "$url"
    [[ -s "$dest" ]] || { echo "错误：$dest 下载为空" >&2; exit 1; }
}

# ---- xtb4stda（GFN 轨道生成，sTDA 前置）----
mkdir -p "$XTB4STDA_HOME/bin"
if [[ -x "$XTB4STDA_HOME/bin/xtb4stda" ]]; then
    echo "（跳过）$XTB4STDA_HOME/bin/xtb4stda 已存在"
else
    fetch "$BASE_URL/xtb4stda" "$XTB4STDA_HOME/bin/xtb4stda"
    chmod +x "$XTB4STDA_HOME/bin/xtb4stda"
fi

# ---- stda（sTDA-xTB 激发态计算；release 资产名 stda_v1.6.1，落位为 stda）----
mkdir -p "$STDA_HOME/bin"
if [[ -x "$STDA_HOME/bin/stda" ]]; then
    echo "（跳过）$STDA_HOME/bin/stda 已存在"
else
    fetch "$BASE_URL/stda_v1.6.1" "$STDA_HOME/bin/stda"
    chmod +x "$STDA_HOME/bin/stda"
fi

# ---- 参数文件（缺一不可，放入 XTB4STDAHOME 约定目录）----
for p in .param_stda1.xtb .param_stda2.xtb; do
    if [[ -s "$XTB4STDA_HOME/$p" ]]; then
        echo "（跳过）$XTB4STDA_HOME/$p 已存在"
    else
        fetch "$PARAM_URL/$p" "$XTB4STDA_HOME/$p"
    fi
done

echo
echo "==> 完成。已安装："
ls -la "$XTB4STDA_HOME" "$XTB4STDA_HOME/bin" "$STDA_HOME/bin"
echo
echo "daemon 启动时自动探测 $XTB4STDA_HOME 并设置 XTB4STDAHOME，excited 工作流开箱即用。"
