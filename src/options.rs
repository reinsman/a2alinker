//! 运行选项的数据结构、默认值、路径处理和校验，不负责读取 a2alinker 配置文件。
//! 库调用方和命令行共同使用这些结构；原生配置与技能直接交给 agent 使用。

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    #[default]
    Pi,
    Codex,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Codex => "codex",
        }
    }
}

// derive 自动实现调试、克隆、默认值和序列化；不需要手写这些重复代码。
// default 使用缺省值补齐字段；deny_unknown_fields 拒绝拼错或不支持的字段。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    pub server: ServerOptions,
    pub identity: IdentityOptions,
    pub agent: AgentOptions,
    pub skills: SkillsOptions,
    pub runtime: RuntimeOptions,
    pub storage: StorageOptions,
    pub auth: AuthOptions,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerOptions {
    pub listen: SocketAddr,
    pub public_url: Option<String>,
    pub max_request_bytes: usize,
}
impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:10100".parse().unwrap(),
            public_url: None,
            max_request_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct IdentityOptions {
    pub name: String,
    /// 空字符串表示启动时读取本地实例的描述；新实例必须提供非空描述。
    pub description: String,
}
impl Default for IdentityOptions {
    fn default() -> Self {
        Self {
            name: "Alan".into(),
            description: "A local coding agent exposed through A2A".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentOptions {
    pub kind: AgentKind,
    pub executable: Option<PathBuf>,
    /// 在同一个 Bash 中准备环境，再 exec agent；None 时直接启动可执行文件。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prepare: Option<String>,
    /// 指定原生配置目录或标准配置文件；None 时完整沿用 agent 的默认发现规则。
    pub native_config: Option<PathBuf>,
    pub workspace: PathBuf,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub allow_extensions: bool,
    /// None 沿用 Codex 原生配置；库调用方可显式覆盖。
    pub sandbox: Option<String>,
    pub approval_policy: Option<String>,
}
impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            kind: AgentKind::Pi,
            executable: None,
            prepare: None,
            native_config: None,
            workspace: ".".into(),
            model: None,
            provider: None,
            args: vec![],
            env: BTreeMap::new(),
            allow_extensions: true,
            sandbox: None,
            approval_policy: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsOptions {
    /// 直接交给原生 agent 的附加技能目录；不复制，也不自动禁用其他原生技能。
    pub directories: Vec<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeOptions {
    pub max_concurrent_tasks: usize,
    pub queue_capacity: usize,
    pub max_sessions: usize,
    pub task_timeout_seconds: u64,
    pub session_idle_timeout_seconds: u64,
    pub startup_timeout_seconds: u64,
    pub cancel_timeout_seconds: u64,
    pub max_protocol_record_bytes: usize,
    pub max_output_bytes: usize,
    /// 是否向客户端转发上游公开的思考文本；不读取隐藏推理或签名。
    pub forward_reasoning: bool,
    /// 是否转发工具调用、参数、执行中的输出和最终结果。
    pub forward_tools: bool,
}
impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            max_concurrent_tasks: 1,
            queue_capacity: 20,
            max_sessions: 16,
            task_timeout_seconds: 1800,
            session_idle_timeout_seconds: 900,
            startup_timeout_seconds: 30,
            cancel_timeout_seconds: 5,
            max_protocol_record_bytes: 8 * 1024 * 1024,
            max_output_bytes: 8 * 1024 * 1024,
            forward_reasoning: true,
            forward_tools: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageOptions {
    /// 共享存储根目录；每个实例的数据位于 directory / identity.name。
    pub directory: PathBuf,
}
impl Default for StorageOptions {
    fn default() -> Self {
        Self {
            directory: "~/.rein/storage".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    #[default]
    None,
    Bearer,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthOptions {
    pub mode: AuthMode,
    pub token_env: String,
}
impl Default for AuthOptions {
    fn default() -> Self {
        Self {
            mode: AuthMode::None,
            token_env: "A2ALINKER_TOKEN".into(),
        }
    }
}

impl Options {
    /// 保持根目录不变，每次由 name 推导实例目录，避免重复 resolve 时追加多次。
    pub fn instance_directory(&self) -> Result<PathBuf> {
        let name = &self.identity.name;
        let mut components = Path::new(name).components();
        ensure!(
            !name.trim().is_empty()
                && matches!(components.next(), Some(Component::Normal(_)))
                && components.next().is_none()
                && !name
                    .chars()
                    .any(|c| matches!(c, '/' | '\\' | ':') || c.is_control()),
            "name must be a single directory name, such as Alan or Bob"
        );
        Ok(self.storage.directory.join(name))
    }

    /// 路径统一相对当前进程目录解析，不读取 a2alinker 配置文件。
    pub fn resolve(&mut self) -> Result<()> {
        self.resolve_paths(&std::env::current_dir()?)?;
        self.validate()
    }

    // &mut self 是独占可变借用：本方法可以改选项，调用方暂时不能同时访问它。
    fn resolve_paths(&mut self, base: &Path) -> Result<()> {
        self.agent.workspace = absolute(&self.agent.workspace, base)?;
        self.storage.directory = absolute(&self.storage.directory, base)?;
        if let Some(path) = &mut self.agent.native_config {
            *path = absolute(path, base)?;
        }
        if let Some(path) = &mut self.agent.executable
            && (path.components().count() > 1 || path.starts_with("~"))
        {
            *path = absolute(path, base)?;
        }
        for path in &mut self.skills.directories {
            *path = absolute(path, base)?;
        }
        Ok(())
    }

    /// 校验跨字段约束；ensure! 在条件不满足时直接返回错误。
    pub fn validate(&self) -> Result<()> {
        self.instance_directory()?;
        if let Some(prepare) = &self.agent.prepare {
            ensure!(!prepare.trim().is_empty(), "--prepare must not be blank");
            ensure!(
                cfg!(unix),
                "--prepare currently requires Unix and /bin/bash"
            );
        }
        ensure!(
            self.identity.description.is_empty() || !self.identity.description.trim().is_empty(),
            "agent description must not be empty"
        );
        ensure!(
            self.agent.workspace.is_dir(),
            "workspace is not a directory: {}",
            self.agent.workspace.display()
        );
        self.native_config_directory()?;
        for p in &self.skills.directories {
            ensure!(
                p.is_dir(),
                "skill directory does not exist: {}",
                p.display()
            );
        }
        ensure!(
            self.runtime.max_concurrent_tasks == 1,
            "this version serializes one workspace; max_concurrent_tasks must be 1"
        );
        ensure!(
            (1..=1000).contains(&self.runtime.queue_capacity),
            "queue_capacity must be 1..=1000"
        );
        ensure!(
            (1..=128).contains(&self.runtime.max_sessions),
            "max_sessions must be 1..=128"
        );
        ensure!(
            self.runtime.task_timeout_seconds > 0
                && self.runtime.session_idle_timeout_seconds > 0
                && self.runtime.startup_timeout_seconds > 0
                && self.runtime.cancel_timeout_seconds > 0,
            "timeouts must be positive"
        );
        ensure!(
            (1024..=64 * 1024 * 1024).contains(&self.runtime.max_protocol_record_bytes),
            "max_protocol_record_bytes must be 1 KiB..=64 MiB"
        );
        ensure!(
            (1024..=64 * 1024 * 1024).contains(&self.runtime.max_output_bytes),
            "max_output_bytes must be 1 KiB..=64 MiB"
        );
        ensure!(
            (1024..=16 * 1024 * 1024).contains(&self.server.max_request_bytes),
            "max_request_bytes must be 1 KiB..=16 MiB"
        );
        if !self.server.listen.ip().is_loopback() {
            ensure!(
                self.auth.mode == AuthMode::Bearer,
                "non-loopback listeners require bearer authentication"
            );
        }
        if let Some(u) = &self.server.public_url {
            let u = url::Url::parse(u).context("invalid public_url")?;
            ensure!(
                matches!(u.scheme(), "http" | "https")
                    && u.host().is_some()
                    && u.username().is_empty()
                    && u.password().is_none()
                    && u.query().is_none()
                    && u.fragment().is_none()
                    && u.path() == "/",
                "public_url must be an http(s) origin without credentials, path, query or fragment"
            );
        }
        ensure!(
            self.agent
                .sandbox
                .as_deref()
                .is_none_or(|value| ["read-only", "workspace-write"].contains(&value)),
            "sandbox must be read-only or workspace-write"
        );
        ensure!(
            self.agent.kind != AgentKind::Pi || self.agent.sandbox.as_deref() != Some("read-only"),
            "pi has no native read-only sandbox; configure tool restrictions or an OS/container boundary explicitly"
        );
        ensure!(
            self.agent.approval_policy.as_deref().is_none_or(|value| [
                "on-request",
                "never",
                "untrusted"
            ]
            .contains(&value)),
            "unsupported approval_policy"
        );
        Ok(())
    }

    pub fn executable(&self) -> PathBuf {
        self.agent
            .executable
            .as_ref()
            .filter(|path| !path.as_os_str().to_string_lossy().trim().is_empty())
            .cloned()
            .unwrap_or_else(|| self.agent.kind.as_str().into())
    }

    /// 只处理显式指定的配置，不探测或读取默认配置，也不读取认证文件。
    /// 原生接口使用目录；标准配置文件通过其所在目录直接使用。
    pub fn native_config_directory(&self) -> Result<Option<PathBuf>> {
        let Some(path) = &self.agent.native_config else {
            return Ok(None);
        };
        if path.is_dir() {
            return Ok(Some(path.clone()));
        }
        ensure!(
            path.is_file(),
            "native configuration does not exist: {}",
            path.display()
        );
        let filename = match self.agent.kind {
            AgentKind::Pi => "settings.json",
            AgentKind::Codex => "config.toml",
        };
        ensure!(
            path.file_name().is_some_and(|name| name == filename),
            "--agent-config for {} must be a directory or a file named {filename}; files are used directly, not copied or renamed",
            self.agent.kind.as_str()
        );
        Ok(Some(
            path.parent()
                .context("native configuration has no parent directory")?
                .to_path_buf(),
        ))
    }

    pub fn public_url(&self) -> String {
        self.server
            .public_url
            .clone()
            .unwrap_or_else(|| format!("http://{}", self.server.listen))
            .trim_end_matches('/')
            .into()
    }
}

pub fn absolute(path: &Path, base: &Path) -> Result<PathBuf> {
    if path == Path::new("~") || path.starts_with("~/") {
        let home = std::env::var_os("HOME").context("HOME is required to expand ~")?;
        return Ok(PathBuf::from(home).join(path.strip_prefix("~")?));
    }
    if path.to_string_lossy().starts_with('~') {
        bail!("only ~ and ~/ paths are supported");
    }
    Ok(if path.is_absolute() {
        path.into()
    } else {
        base.join(path)
    })
}
