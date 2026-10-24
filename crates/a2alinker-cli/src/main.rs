//! 二进制入口：定义和解析命令行参数、初始化日志、处理进程信号，再调用库启动服务。

use std::{net::SocketAddr, path::PathBuf};

use a2alinker::{AgentKind, Options, options::AuthMode};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

// 普通 Rust main 不能直接等待异步操作；此宏创建 Tokio 运行时来执行 async main。
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 日志使用 stderr，避免与命令的标准输出混在一起。RUST_LOG 可覆盖默认级别。
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "a2alinker=info".into()),
        )
        .init();
    let options = Args::parse().into_options()?;
    let mut server = a2alinker::Server::start(options).await?;
    tokio::select! {
        result = server.wait() => result,
        _ = signal() => server.shutdown().await,
    }
}

async fn signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "a2alinker",
    version,
    about = "将本地 pi 或 Codex 提供为 A2A 服务",
    arg_required_else_help = true
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 启动服务；省略 AGENT 时默认使用 pi。
    Serve(Box<ServeArgs>),
}

#[derive(Debug, clap::Args)]
struct ServeArgs {
    /// Agent 类型；也可通过 --agent 指定。
    #[arg(value_enum, value_name = "AGENT")]
    backend: Option<AgentArg>,
    /// Agent 类型，默认 pi；位置参数优先。
    #[arg(long, value_enum, env = "A2ALINKER_AGENT")]
    agent: Option<AgentArg>,
    /// 必填的实例名字，也是 Agent Card 名字，例如 Alan、Bob。
    #[arg(long, env = "A2ALINKER_NAME")]
    name: String,
    /// Agent Card 描述；新实例必填，已有实例省略时沿用本地描述。
    #[arg(long, env = "A2ALINKER_DESC", value_parser = parse_description)]
    desc: Option<String>,
    /// 可执行文件；省略或空白时从 PATH 查找 pi / codex。
    #[arg(long, env = "A2ALINKER_EXECUTABLE")]
    executable: Option<PathBuf>,
    /// 每次启动原生进程前在同一个 Bash 中执行；可 source 环境脚本、切换 nvm、export 代理。
    #[arg(long, env = "A2ALINKER_PREPARE", hide_env_values = true)]
    prepare: Option<String>,
    /// 原生配置目录或标准文件 settings.json / config.toml；直接使用，不复制。
    #[arg(long, env = "A2ALINKER_AGENT_CONFIG")]
    agent_config: Option<PathBuf>,
    /// 附加技能目录，可重复；直接加载源目录，省略时沿用原生发现规则。
    #[arg(long, env = "A2ALINKER_SKILL_DIRS", value_delimiter = ';')]
    skill_dir: Vec<PathBuf>,
    /// Agent 工作目录，默认当前进程的工作目录。
    #[arg(long, env = "A2ALINKER_WORKSPACE", default_value = ".")]
    workspace: PathBuf,
    /// 端口或 IP:端口；占用时向上递增，端口 0 由系统分配。
    #[arg(long, env = "A2ALINKER_LISTEN", default_value = "10100", value_parser = parse_listen)]
    listen: SocketAddr,
    /// 对外公开的 HTTP(S) origin；省略时使用实际监听地址。
    #[arg(long, env = "A2ALINKER_PUBLIC_URL")]
    public_url: Option<String>,
    /// 共享存储根目录，实例数据保存在该目录下的 {name}/ 中。
    #[arg(long, env = "A2ALINKER_STORAGE_DIR", default_value = "~/.rein/storage")]
    storage_dir: PathBuf,
    /// 覆盖原生模型选择。
    #[arg(long, env = "A2ALINKER_MODEL")]
    model: Option<String>,
    /// 覆盖原生 provider 选择。
    #[arg(long, env = "A2ALINKER_PROVIDER")]
    provider: Option<String>,
    /// A2A 认证方式；bearer 凭据通过 A2ALINKER_TOKEN 提供。
    #[arg(long, value_enum, env = "A2ALINKER_AUTH", default_value = "none")]
    auth: AuthArg,
}

// 参数枚举只属于命令行包；转换成库的类型后再调用服务 API。
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
enum AgentArg {
    #[default]
    Pi,
    Codex,
}

impl From<AgentArg> for AgentKind {
    fn from(value: AgentArg) -> Self {
        match value {
            AgentArg::Pi => Self::Pi,
            AgentArg::Codex => Self::Codex,
        }
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum AuthArg {
    None,
    Bearer,
}

impl From<AuthArg> for AuthMode {
    fn from(value: AuthArg) -> Self {
        match value {
            AuthArg::None => Self::None,
            AuthArg::Bearer => Self::Bearer,
        }
    }
}

impl Args {
    fn into_options(self) -> Result<Options> {
        let Command::Serve(args) = self.command;
        args.into_options()
    }
}

impl ServeArgs {
    fn into_options(self) -> Result<Options> {
        let mut options = Options::default();
        options.agent.kind = self.backend.or(self.agent).unwrap_or_default().into();
        options.agent.executable = self
            .executable
            .filter(|path| !path.as_os_str().to_string_lossy().trim().is_empty());
        options.agent.native_config = self.agent_config;
        options.agent.prepare = self.prepare;
        options.agent.workspace = self.workspace;
        options.agent.model = self.model;
        options.agent.provider = self.provider;
        options.identity.name = self.name;
        options.identity.description = self.desc.unwrap_or_default();
        options.server.listen = self.listen;
        options.server.public_url = self.public_url;
        options.storage.directory = self.storage_dir;
        options.auth.mode = self.auth.into();
        if options.auth.mode == AuthMode::Bearer {
            options.auth.token = Some(
                std::env::var("A2ALINKER_TOKEN")
                    .context("--auth bearer requires the A2ALINKER_TOKEN environment variable")?,
            );
        }
        if !self.skill_dir.is_empty() {
            options.skills.directories = self.skill_dir;
        }
        options.resolve()?;
        Ok(options)
    }
}

fn parse_description(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("描述不能为空或只有空白字符".into())
    } else {
        Ok(value.into())
    }
}

fn parse_listen(value: &str) -> Result<SocketAddr, String> {
    if let Ok(port) = value.parse::<u16>() {
        Ok(SocketAddr::from(([127, 0, 0, 1], port)))
    } else {
        value
            .parse()
            .map_err(|_| "listen 必须是 0..65535 的端口或 IP:端口".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn only_serve_and_help_are_available() {
        for args in [
            vec!["a2alinker", "--help"],
            vec!["a2alinker", "serve", "--help"],
            vec!["a2alinker", "help", "serve"],
        ] {
            assert_eq!(
                Args::try_parse_from(args).unwrap_err().kind(),
                ErrorKind::DisplayHelp
            );
        }
        for command in ["doctor", "config", "skills", "agents"] {
            assert!(Args::try_parse_from(["a2alinker", command]).is_err());
        }
        assert!(Args::try_parse_from(["a2alinker", "serve", "--config", "old.toml"]).is_err());
        assert!(Args::try_parse_from(["a2alinker", "serve", "other-agent"]).is_err());
    }

    #[test]
    fn positional_and_flag_agent_selection_are_supported() {
        for (mut args, expected) in [
            (vec!["a2alinker", "serve", "pi"], AgentKind::Pi),
            (vec!["a2alinker", "serve", "codex"], AgentKind::Codex),
            (
                vec!["a2alinker", "serve", "--agent", "codex"],
                AgentKind::Codex,
            ),
        ] {
            args.extend(["--name", "Alan"]);
            assert_eq!(
                Args::try_parse_from(args)
                    .unwrap()
                    .into_options()
                    .unwrap()
                    .agent
                    .kind,
                expected
            );
        }
    }

    #[test]
    fn name_is_required_and_explicit_descriptions_must_not_be_blank() {
        assert_eq!(
            Args::try_parse_from(["a2alinker", "serve"])
                .unwrap_err()
                .kind(),
            ErrorKind::MissingRequiredArgument
        );
        for description in ["", " ", "\t"] {
            assert_eq!(
                Args::try_parse_from([
                    "a2alinker",
                    "serve",
                    "--name",
                    "Alan",
                    "--desc",
                    description
                ])
                .unwrap_err()
                .kind(),
                ErrorKind::ValueValidation
            );
        }
    }

    #[test]
    fn listen_accepts_ports_and_full_addresses() {
        assert_eq!(
            parse_listen("10100").unwrap(),
            "127.0.0.1:10100".parse().unwrap()
        );
        assert_eq!(parse_listen("0").unwrap().port(), 0);
        assert_eq!(
            parse_listen("[::1]:10100").unwrap(),
            "[::1]:10100".parse().unwrap()
        );
        assert!(parse_listen("65536").is_err());
    }
}
