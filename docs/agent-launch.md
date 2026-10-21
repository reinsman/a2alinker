# agent 启动环境与 nvm

a2alinker 使用 Rust 的子进程 API 直接执行 `agent.executable`，默认是 `pi` 或 `codex`。它继承启动进程的环境，再应用 `agent.env`，不会自动读取 `.zshrc`、`.bashrc`，也不会执行 shell 函数。

`nvm use 24.20.0` 会改变当前 shell 的 PATH，让它找到该版本的 `node` 和该版本下全局安装的 `pi`。尚未切换版本的终端、桌面程序或服务启动的 a2alinker，可能找不到相同的程序。nvm 的运行方式可参考[官方文档](https://github.com/nvm-sh/nvm#about)。

## 手动启动

如果只在当前终端使用，可以先切换版本再启动，a2alinker 和 agent 都会继承该环境：

```bash
nvm use 24.20.0
command -v node
command -v pi
./target/release/a2alinker serve --config examples/pi.toml
```

## 在配置中选择 Node.js 版本

仓库提供了 [pi-nvm 启动脚本](../scripts/pi-nvm) 和 [完整配置示例](../examples/pi-nvm.toml)，适用于 Linux、macOS 和已安装 nvm 的 WSL：

```bash
./target/release/a2alinker doctor --config examples/pi-nvm.toml
./target/release/a2alinker serve --config examples/pi-nvm.toml
```

已有配置只需在相应位置设置以下字段；不要重复声明已有的 `[agent]` 表：

```toml
[agent]
kind = "pi"
executable = "../scripts/pi-nvm"

[agent.env]
A2ALINKER_NODE_VERSION = "24.20.0"
# NVM_DIR = "/absolute/path/to/nvm"
```

`executable` 的相对路径以配置文件所在目录为基准；上例适用于配置放在仓库 `examples/` 中的情况。部署时应同时部署脚本，也可改为它的绝对路径。Git 会保留脚本的可执行权限；手动复制后若权限丢失，执行 `chmod +x scripts/pi-nvm`。

脚本先读取 `NVM_DIR/nvm.sh`，只加载 nvm，再执行 `nvm use --silent` 和 `exec pi`。默认版本是 `24.20.0`，`A2ALINKER_NODE_VERSION` 可覆盖它。未指定 `NVM_DIR` 时，使用 `$XDG_CONFIG_HOME/nvm`（若设置了 XDG_CONFIG_HOME），否则使用 `$HOME/.nvm`。

脚本只选择已安装的 Node.js 和该版本下的 pi，不自动安装软件，也不修改父进程或其他终端的环境。初始化日志送往 stderr，不混入 pi 的 stdout JSONL 协议；`exec` 保留 agent 进程组与标准输入输出，任务取消和进程清理仍由网关管理。

`doctor` 的版本探测、协议握手以及 `serve` 创建的会话都会使用同一个启动脚本。它不需要先在终端执行 `nvm use`。修改启动程序或 `agent.env` 后，执行指纹会变化，已有 context 需要重新创建。

## 直接配置绝对路径与 PATH

不使用脚本时，也可以同时指定 pi 路径和包含正确 node 的 PATH，例如：

```toml
[agent]
kind = "pi"
executable = "/Users/xhb/.nvm/versions/node/v24.20.0/bin/pi"

[agent.env]
PATH = "/Users/xhb/.nvm/versions/node/v24.20.0/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
```

请按实际用户目录和工具链调整完整 PATH。pi 的入口使用 `#!/usr/bin/env node`，所以仅指定 pi 的绝对路径仍可能找到错误版本的 node。还需保留 agent 使用的 git、Python 等工具所在目录。

`agent.env` 的值按字面传给子进程，不展开 `$PATH`、`$HOME` 或 `~`；`executable` 也不是 shell 命令，不能写成 `"nvm use 24.20.0 && pi"`。需要初始化逻辑时使用启动脚本。

## 排查启动失败

- `cannot start agent executable`：检查可执行文件是否存在、是否有执行权限，以及脚本首行的解释器是否存在。只写程序名称时，检查子进程 PATH。
- `agent --version exited unsuccessfully`：程序已经启动但返回失败。可在终端直接执行 `scripts/pi-nvm --version` 查看原始诊断；网关不会把 agent stderr 原文写入服务日志，以免暴露凭据。
- 提示版本或 pi 未安装：先在终端执行 `nvm install 24.20.0`、`nvm use 24.20.0`，再执行 `npm install -g @earendil-works/pi-coding-agent`。全局 npm 包随 nvm 的 Node.js 版本分别安装。
