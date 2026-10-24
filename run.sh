#!/bin/bash
# 使用已构建的 debug 版本启动本地 Pi；可追加 --name、--desc、--listen 等参数。

project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)" || exit 1
binary="$project_dir/target/debug/a2alinker"
if [ ! -x "$binary" ]; then
    printf '请先在项目目录执行 cargo build\n' >&2
    exit 1
fi

# 通过环境变量提供默认值，命令行参数仍然优先。
export A2ALINKER_NAME="${A2ALINKER_NAME:-Alan}"
export A2ALINKER_DESC="${A2ALINKER_DESC:-本地 Pi 编程助手}"
export A2ALINKER_WORKSPACE="${A2ALINKER_WORKSPACE:-$project_dir}"
export A2ALINKER_PREPARE='
    export NVM_DIR="${NVM_DIR:-$HOME/.nvm}" &&
    source "$NVM_DIR/nvm.sh" --no-use &&
    nvm use node
'

exec "$binary" serve pi "$@"
