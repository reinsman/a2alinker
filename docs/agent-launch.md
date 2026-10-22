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

## 使用 pi 的默认配置文件

`pi.toml` 和 `pi-nvm.toml` 默认不再指定仓库里的空 `settings.json`。省略 `agent.native_config` 时，自动使用：

1. `PI_CODING_AGENT_DIR/settings.json`，如果设置了该环境变量。
2. 否则使用 `$HOME/.pi/agent/settings.json`。

`agent.env` 中的环境变量覆盖项优先于启动网关的环境变量；相对配置目录以 agent 的 workspace 为基准。默认文件不存在时使用 pi 内置默认值。

如需固定到某个文件，仍可显式填写：

```toml
[agent]
kind = "pi"
workspace = ".."
native_config = "~/.pi/agent/settings.json"
```

显式路径优先于自动发现，文件不存在会报错。`native_config` 支持展开 `~/`；`agent.env` 中的普通字符串不会做 shell 展开。也可以显式指向仓库中的空文件，以选择不继承用户设置。

运行 `config validate --config examples/pi-nvm.toml` 或 `doctor --config examples/pi-nvm.toml`，输出的 `Native configuration` 会显示实际来源。默认配置复制到实例私有目录后再由 pi 读取，原文件不被修改。配置内容变化会更新执行指纹；重启后需新建 context 使用新配置。

自动继承全局 `settings.json` 和同目录的 `models.json`（如果存在）；后者保存自定义 provider 的端点、API、模型定义等。即使默认 settings 文件不存在，也会继承 models 文件。显式指定单个 settings 文件时会携带其同目录的 models 文件；如需其他配套文件，可显式指定专用配置目录。登录文件由 `inherit_auth` 控制，技能和扩展加载仍由 a2alinker 对应配置控制。

`models.json` 可能含密钥，其副本权限为 `0600`，内容不会打印到日志。它的内容变化也会更新执行指纹。`doctor` 和 `config validate` 会显示 `Native model configuration` 来源，pi 握手日志会显示实际选中的 provider 和模型名称。

## 会话文件与 forbidden 的排查

`data/pi-reviewer/sessions/` 中只有一个 JSONL 文件，不代表会话丢失。一个 context 对应一个文件，后续任务带同一 contextId 就会复用它。pi 通常在首次 assistant 消息后才写入新会话文件；启动预检和 doctor 不会生成完整对话记录。要保留历史会话，应持久化整个 storage 目录，不能只保留数据库。

如果 curl 返回 HTTP 200，但 Task 状态是 `TASK_STATE_FAILED`，状态消息含 `403 forbidden`，说明请求已到达网关，模型服务拒绝了后续请求。先检查 pi 握手日志中的 provider/model 是否符合预期，以及 models 配置是否加载；若选择正确但仍被拒绝，需要检查该模型服务的凭据、账号权限和请求策略。网关 bearer 认证失败会直接返回 HTTP 401。

例如上游返回 `permission_denied / plan_model_not_allowed`，并提示 `this project's plan is not authorized for this model`，表示该项目套餐没有所选模型的调用权限。应选择账号允许的模型，或在模型服务端处理授权；修改网关 bearer 配置无法解决。`doctor` 只验证原生握手、技能加载和模型选择，不发送模型请求，因此不能验证远端套餐权限。

更新配置或二进制后应重启正在运行的进程；已经启动的进程不会自动读取新代码。配置来源或 models 内容变化后需使用新的 context。保留原 task 查询历史；修改消息内容时应使用新的 messageId，避免去重机制返回原来的失败任务。

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
