//! 命令行入口：解析配置覆盖项，分派检查命令，并管理 HTTP 服务的启动和退出。

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::{
    a2a,
    adapters::{self, AdapterContext},
    config::{AgentKind, AuthMode, Config, absolute},
    runtime::Gateway,
    skills::SkillSet,
};

// derive 让 clap 自动生成参数解析逻辑；下方 command/arg 属性定义命令行格式。
#[derive(Debug, Parser)]
#[command(
    name = "a2alinker",
    version,
    about = "Expose pi or Codex as an A2A agent"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 启动 A2A JSON-RPC + SSE 服务。
    Serve(Box<CommonArgs>),
    /// 检查可执行文件并完成协议握手，不发起模型推理。
    Doctor(Box<CommonArgs>),
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Agents {
        #[command(subcommand)]
        command: AgentsCommand,
    },
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    Validate(Box<CommonArgs>),
}
#[derive(Debug, Subcommand)]
pub enum AgentsCommand {
    List,
}
#[derive(Debug, Subcommand)]
pub enum SkillsCommand {
    List(Box<CommonArgs>),
}

#[derive(Debug, Default, Args)]
pub struct CommonArgs {
    // Option<T> 表示参数可以不提供；只有 Some(value) 才覆盖配置文件中的值。
    #[arg(long, env = "A2ALINKER_CONFIG")]
    pub config: Option<PathBuf>,
    #[arg(long, value_enum, env = "A2ALINKER_AGENT")]
    pub agent: Option<AgentKind>,
    #[arg(long, env = "A2ALINKER_EXECUTABLE")]
    pub executable: Option<PathBuf>,
    #[arg(long, env = "A2ALINKER_AGENT_CONFIG")]
    pub agent_config: Option<PathBuf>,
    /// 可重复指定技能根目录；会替换配置文件中的目录列表。
    #[arg(long, env = "A2ALINKER_SKILL_DIRS", value_delimiter = ';')]
    pub skill_dir: Vec<PathBuf>,
    #[arg(long, env = "A2ALINKER_WORKSPACE")]
    pub workspace: Option<PathBuf>,
    #[arg(long, env = "A2ALINKER_LISTEN")]
    pub listen: Option<SocketAddr>,
    #[arg(long, env = "A2ALINKER_PUBLIC_URL")]
    pub public_url: Option<String>,
    #[arg(long, env = "A2ALINKER_STORAGE_DIR")]
    pub storage_dir: Option<PathBuf>,
    #[arg(long, env = "A2ALINKER_MODEL")]
    pub model: Option<String>,
    #[arg(long, env = "A2ALINKER_PROVIDER")]
    pub provider: Option<String>,
    #[arg(long, value_enum, env = "A2ALINKER_AUTH")]
    pub auth: Option<AuthMode>,
}

impl CommonArgs {
    /// 先加载 TOML，再应用命令行/环境变量覆盖项，最后统一校验。
    pub fn resolve(&self) -> Result<Config> {
        let mut cfg = Config::load(self.config.as_deref())?;
        let cwd = std::env::current_dir()?;
        if let Some(kind) = self.agent {
            cfg.agent.kind = kind;
        }
        if let Some(path) = &self.executable {
            cfg.agent.executable = Some(if path.components().count() > 1 {
                absolute(path, &cwd)?
            } else {
                path.clone()
            });
        }
        if let Some(path) = &self.agent_config {
            cfg.agent.native_config = Some(absolute(path, &cwd)?);
        }
        if let Some(path) = &self.workspace {
            cfg.agent.workspace = absolute(path, &cwd)?;
        }
        if let Some(path) = &self.storage_dir {
            cfg.storage.directory = absolute(path, &cwd)?;
        }
        if !self.skill_dir.is_empty() {
            cfg.skills.directories = self
                .skill_dir
                .iter()
                .map(|p| absolute(p, &cwd))
                .collect::<Result<_>>()?;
        }
        if let Some(listen) = self.listen {
            cfg.server.listen = listen;
        }
        if let Some(url) = &self.public_url {
            cfg.server.public_url = Some(url.clone());
        }
        if let Some(model) = &self.model {
            cfg.agent.model = Some(model.clone());
        }
        if let Some(provider) = &self.provider {
            cfg.agent.provider = Some(provider.clone());
        }
        if let Some(mode) = &self.auth {
            cfg.auth.mode = mode.clone();
        }
        cfg.validate()?;
        Ok(cfg)
    }
}

pub async fn execute(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Serve(args) => serve(args.resolve()?).await,
        Command::Doctor(args) => {
            let mut config = args.resolve()?;
            let version = adapters::probe(&config).await?;
            println!("Executable: {version}");
            println!(
                "Native configuration: {}",
                native_configuration_label(&config)?
            );
            if let Some(path) = config.native_model_config()? {
                println!("Native model configuration: {}", path.display());
            }
            // 临时目录拥有自己的生命周期；离开此分支时会自动删除，不使用正式存储。
            let temp = tempfile::tempdir()?;
            config.storage.directory = temp.path().into();
            let skills = SkillSet::discover(&config)?.snapshot(temp.path())?;
            let skill_count = skills.skills.len();
            let home = adapters::prepare_home(&config, &skills.digest)?;
            let seconds = config.runtime.startup_timeout_seconds;
            let context = AdapterContext {
                config: Arc::new(config),
                skills,
                home,
            };
            // 只创建会话、握手和验证技能，不调用 session.run()，因此不会发送推理任务。
            let session = tokio::time::timeout(
                Duration::from_secs(seconds),
                adapters::create_session(&context, &uuid::Uuid::new_v4().to_string(), None),
            )
            .await
            // 两个 ? 分别处理 timeout 的外层错误、create_session 的内层错误。
            .context("protocol handshake timed out")??;
            drop(session);
            println!(
                "Protocol handshake: OK\nSelected skills: {skill_count}\nNo inference was requested."
            );
            Ok(())
        }
        Command::Config {
            command: ConfigCommand::Validate(args),
        } => {
            let config = args.resolve()?;
            let skills = SkillSet::discover(&config)?;
            println!(
                "Configuration: OK\nAgent: {}\nWorkspace: {}\nNative configuration: {}\nSelected skills: {}",
                config.agent.kind.as_str(),
                config.agent.workspace.display(),
                native_configuration_label(&config)?,
                skills.skills.len()
            );
            if let Some(path) = config.native_model_config()? {
                println!("Native model configuration: {}", path.display());
            }
            Ok(())
        }
        Command::Agents {
            command: AgentsCommand::List,
        } => {
            println!(
                "pi     RPC over stdio; sessions, streaming, cancellation, extension input\ncodex  app-server over stdio; threads, streaming, cancellation, approval/input"
            );
            Ok(())
        }
        Command::Skills {
            command: SkillsCommand::List(args),
        } => {
            let skills = SkillSet::discover(&args.resolve()?)?;
            println!("{}", serde_json::to_string_pretty(&skills.skills)?);
            Ok(())
        }
    }
}

// 只显示配置来源路径，不输出配置内容，便于确认当前使用的是哪个原生文件。
fn native_configuration_label(config: &Config) -> Result<String> {
    Ok(config
        .effective_native_config()?
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "agent built-in defaults".into()))
}

/// 绑定端口、预检 agent，再运行 HTTP；收到退出信号后等待任务清理完成。
async fn serve(mut config: Config) -> Result<()> {
    let version = adapters::probe(&config).await?;
    tracing::info!(agent = config.agent.kind.as_str(), %version, "agent executable detected");
    let listener = tokio::net::TcpListener::bind(config.server.listen).await?;
    // 当配置端口为 0 时，写回系统实际分配的端口，保证 Agent Card 中地址正确。
    config.server.listen = listener.local_addr()?;
    let gateway = Gateway::open(config).await?;
    let router = match a2a::router(gateway.clone()) {
        Ok(router) => router,
        Err(error) => {
            gateway.shutdown().await;
            return Err(error);
        }
    };
    tracing::info!(listen = %listener.local_addr()?, agent = gateway.config.agent.kind.as_str(), "A2A server ready");
    tracing::info!(
        event = "a2a.endpoints", protocol_version = "1.0", transport = "JSONRPC",
        discovery_url = format!("{}/.well-known/agent-card.json", gateway.config.public_url()),
        rpc_url = format!("{}/a2a", gateway.config.public_url()),
        auth = ?gateway.config.auth.mode,
        "服务入口：GET Agent Card 发现能力，POST /a2a 提交消息；流式方法通过 SSE 返回事件"
    );
    let shutdown = gateway.clone();
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            signal().await;
            shutdown.shutdown().await;
        })
        .await;
    gateway.shutdown().await;
    result.context("A2A server failed")
}

// cfg 是条件编译：Unix 同时支持 SIGTERM 和 Ctrl+C，其他平台只编译 Ctrl+C 分支。
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
