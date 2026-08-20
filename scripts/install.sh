#!/usr/bin/env bash
# xTB-Pilot 用户级安装：release 构建并安装 xtbp-tui / xtbp-daemon 到 ~/.local/bin
# （面向人类用户的即输即用：任意目录输入 `xtbp-tui` 即可，daemon 会自动拉起）
set -euo pipefail

cd "$(dirname "$0")/.."
BIN_DIR="${1:-$HOME/.local/bin}"

echo "==> 构建 release（xtbp-tui + xtbp-daemon）"
cargo build --release -p xtbp-tui -p xtbp-daemon

echo "==> 安装到 $BIN_DIR"
mkdir -p "$BIN_DIR"
cp target/release/xtbp-tui "$BIN_DIR/"
cp target/release/xtbp-daemon "$BIN_DIR/"
chmod +x "$BIN_DIR/xtbp-tui" "$BIN_DIR/xtbp-daemon"

echo "==> 完成"
"$BIN_DIR/xtbp-tui" --version
echo "现在可以在任意目录直接输入: xtbp-tui"

if [[ ":$PATH:" != *":$BIN_DIR:"* ]]; then
    cat <<EOF

⚠  $BIN_DIR 不在 PATH 中。请把下面一行加入 ~/.bashrc 后重新登录：
    export PATH="\$HOME/.local/bin:\$PATH"
EOF
else
    echo "（$BIN_DIR 已在 PATH 中，无需额外配置）"
fi
