//! 二进制专用的参数解析；服务实现位于 lib 的 Server、runtime 和 adapters。

use std::{net::SocketAddr, path::PathBuf};

use a2alinker::{AgentKind, Options, options::AuthMode};
use anyhow::Result;
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "a2alinker",
    version,
    about = "将本地 pi 或 Codex 提供为 A2A 服务",
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 启动服务；省略 AGENT 时默认使用 pi。
    Serve(Box<ServeArgs>),
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Agent 类型；也可通过 --agent 指定。
    #[arg(value_enum, value_name = "AGENT")]
    pub backend: Option<AgentKind>,
    /// Agent 类型，默认 pi；位置参数优先。
    #[arg(long, value_enum, env = "A2ALINKER_AGENT")]
    pub agent: Option<AgentKind>,
    /// Agent Card 中的简短名字，例如 Alan、Bob。
    #[arg(long, env = "A2ALINKER_NAME", default_value = "Alan")]
    pub name: String,
    /// Agent Card 中的能力描述。
    #[arg(long, env = "A2ALINKER_DESC")]
    pub desc: Option<String>,
    /// 可执行文件；省略或空白时从 PATH 查找 pi / codex。
    #[arg(long, env = "A2ALINKER_EXECUTABLE")]
    pub executable: Option<PathBuf>,
    /// 原生 settings.json、config.toml 或配置目录；省略时使用 agent 默认配置。
    #[arg(long, env = "A2ALINKER_AGENT_CONFIG")]
    pub agent_config: Option<PathBuf>,
    /// 技能目录，可重复；省略时使用对应 agent 原生的 skills 目录和发现规则。
    #[arg(long, env = "A2ALINKER_SKILL_DIRS", value_delimiter = ';')]
    pub skill_dir: Vec<PathBuf>,
    /// Agent 工作目录，默认当前进程的工作目录。
    #[arg(long, env = "A2ALINKER_WORKSPACE", default_value = ".")]
    pub workspace: PathBuf,
    /// 端口或 IP:端口；占用时向上递增，端口 0 由系统分配。
    #[arg(long, env = "A2ALINKER_LISTEN", default_value = "10100", value_parser = parse_listen)]
    pub listen: SocketAddr,
    /// 对外公开的 HTTP(S) origin；省略时使用实际监听地址。
    #[arg(long, env = "A2ALINKER_PUBLIC_URL")]
    pub public_url: Option<String>,
    /// 共享存储根目录，实例数据保存在该目录下的 {name}/ 中。
    #[arg(long, env = "A2ALINKER_STORAGE_DIR", default_value = "~/.rein/storage")]
    pub storage_dir: PathBuf,
    /// 覆盖原生模型选择。
    #[arg(long, env = "A2ALINKER_MODEL")]
    pub model: Option<String>,
    /// 覆盖原生 provider 选择。
    #[arg(long, env = "A2ALINKER_PROVIDER")]
    pub provider: Option<String>,
    /// A2A 认证方式；bearer 凭据通过 A2ALINKER_TOKEN 提供。
    #[arg(long, value_enum, env = "A2ALINKER_AUTH", default_value = "none")]
    pub auth: AuthMode,
}

impl Cli {
    pub fn into_options(self) -> Result<Options> {
        let Command::Serve(args) = self.command;
        args.into_options()
    }
}

impl ServeArgs {
    fn into_options(self) -> Result<Options> {
        let mut options = Options::default();
        options.agent.kind = self.backend.or(self.agent).unwrap_or_default();
        options.agent.executable = self
            .executable
            .filter(|path| !path.as_os_str().to_string_lossy().trim().is_empty());
        options.agent.native_config = self.agent_config;
        options.agent.workspace = self.workspace;
        options.agent.model = self.model;
        options.agent.provider = self.provider;
        options.identity.name = self.name;
        options.identity.description = self
            .desc
            .unwrap_or_else(|| format!("本地 {} 编程助手", options.agent.kind.as_str()));
        options.server.listen = self.listen;
        options.server.public_url = self.public_url;
        options.storage.directory = self.storage_dir;
        options.auth.mode = self.auth;
        if !self.skill_dir.is_empty() {
            options.skills.directories = self.skill_dir;
            options.skills.inherit_user = false;
        }
        options.resolve()?;
        Ok(options)
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
                Cli::try_parse_from(args).unwrap_err().kind(),
                ErrorKind::DisplayHelp
            );
        }
        for command in ["doctor", "config", "skills", "agents"] {
            assert!(Cli::try_parse_from(["a2alinker", command]).is_err());
        }
        assert!(Cli::try_parse_from(["a2alinker", "serve", "--config", "old.toml"]).is_err());
        assert!(Cli::try_parse_from(["a2alinker", "serve", "other-agent"]).is_err());
    }

    #[test]
    fn positional_and_flag_agent_selection_are_supported() {
        for (args, expected) in [
            (vec!["a2alinker", "serve", "pi"], AgentKind::Pi),
            (vec!["a2alinker", "serve", "codex"], AgentKind::Codex),
            (
                vec!["a2alinker", "serve", "--agent", "codex"],
                AgentKind::Codex,
            ),
        ] {
            assert_eq!(
                Cli::try_parse_from(args)
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
