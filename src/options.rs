//! 运行选项的数据结构、默认值、路径处理和校验，不负责读取 a2alinker 配置文件。
//! 库调用方和命令行共同使用这些结构；执行指纹用于判断旧会话能否安全复用。

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
    /// 显式覆盖原生配置；默认分别查找用户的 settings.json 或 config.toml。
    pub native_config: Option<PathBuf>,
    pub workspace: PathBuf,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub inherit_auth: bool,
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
            native_config: None,
            workspace: ".".into(),
            model: None,
            provider: None,
            args: vec![],
            env: BTreeMap::new(),
            inherit_auth: true,
            allow_extensions: true,
            sandbox: None,
            approval_policy: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsOptions {
    pub directories: Vec<PathBuf>,
    pub enabled: Vec<String>,
    pub inherit_user: bool,
}

impl Default for SkillsOptions {
    fn default() -> Self {
        // 默认沿用 agent 原生技能发现规则，显式技能目录由调用方设置。
        Self {
            directories: vec![],
            enabled: vec![],
            inherit_user: true,
        }
    }
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
        ensure!(
            !self.identity.description.trim().is_empty(),
            "agent description must not be empty"
        );
        ensure!(
            self.agent.workspace.is_dir(),
            "workspace is not a directory: {}",
            self.agent.workspace.display()
        );
        if let Some(p) = self.effective_native_config()? {
            ensure!(
                p.exists(),
                "native configuration does not exist: {}",
                p.display()
            );
        }
        self.native_model_config()?;
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

    /// 原始用户配置目录。先取 agent.env 中的覆盖项，再取启动网关时的环境变量。
    /// agent 真正启动时会使用实例私有目录，这里保留的是复制配置和凭据的来源。
    pub fn user_agent_directory(&self) -> Result<Option<PathBuf>> {
        let env = |key: &str| {
            self.agent
                .env
                .get(key)
                .map(std::ffi::OsString::from)
                .or_else(|| std::env::var_os(key))
                .filter(|value| !value.is_empty())
        };
        let (key, fallback) = match self.agent.kind {
            AgentKind::Pi => ("PI_CODING_AGENT_DIR", ".pi/agent"),
            AgentKind::Codex => ("CODEX_HOME", ".codex"),
        };
        env(key)
            .map(PathBuf::from)
            .or_else(|| env("HOME").map(|home| PathBuf::from(home).join(fallback)))
            .map(|directory| absolute(&directory, &self.agent.workspace))
            .transpose()
    }

    /// 显式 native_config 优先；pi 省略时读取用户 settings.json，
    /// Codex 默认读取用户 config.toml，以保留模型、技能、沙箱与审批设置。
    /// 默认文件不存在则使用 agent 内置默认值。
    /// 不修改配置字段，确保 CLI 在覆盖 agent 类型后仍能选到正确的默认文件。
    pub fn effective_native_config(&self) -> Result<Option<PathBuf>> {
        if let Some(path) = &self.agent.native_config {
            return Ok(Some(path.clone()));
        }
        let filename = match self.agent.kind {
            AgentKind::Pi => "settings.json",
            AgentKind::Codex => "config.toml",
        };
        if let Some(directory) = self.user_agent_directory()? {
            let path = directory.join(filename);
            if path
                .try_exists()
                .with_context(|| format!("check default agent configuration {}", path.display()))?
            {
                ensure!(
                    path.is_file(),
                    "default agent configuration is not a file: {}",
                    path.display()
                );
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    /// pi 的自定义 provider/模型定义。单文件配置同时携带其同目录的 models.json；
    /// 省略配置时读取用户默认目录，显式目录则已由完整目录复制流程处理。
    pub fn native_model_config(&self) -> Result<Option<PathBuf>> {
        if self.agent.kind != AgentKind::Pi {
            return Ok(None);
        }
        let directory = match &self.agent.native_config {
            Some(path) if path.is_dir() => return Ok(None),
            Some(path) => path.parent().map(Path::to_path_buf),
            None => self.user_agent_directory()?,
        };
        if let Some(directory) = directory {
            let path = directory.join("models.json");
            if path
                .try_exists()
                .with_context(|| format!("check pi model configuration {}", path.display()))?
            {
                ensure!(
                    path.is_file(),
                    "pi model configuration is not a file: {}",
                    path.display()
                );
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    pub fn public_url(&self) -> String {
        self.server
            .public_url
            .clone()
            .unwrap_or_else(|| format!("http://{}", self.server.listen))
            .trim_end_matches('/')
            .into()
    }

    /// 对执行配置、原生配置文件和技能摘要取哈希，防止旧 context 静默换用新配置。
    pub fn fingerprint(&self, skill_digest: &str) -> Result<String> {
        // 只有执行设置影响会话兼容性；其中的凭据参与哈希，但不输出明文到日志。
        let content = serde_json::to_vec(&(&self.agent, &self.skills, skill_digest))?;
        let mut digest = Sha256::new();
        digest.update(content);
        if let Some(path) = self.effective_native_config()? {
            // 自动发现的路径不在 agent.native_config 字段中，也需要纳入执行指纹。
            if self.agent.native_config.is_none() {
                digest.update(serde_json::to_vec(&path)?);
            }
            if path.is_file() {
                digest.update(std::fs::read(path)?);
            } else {
                let mut total = 0u64;
                for entry in walkdir::WalkDir::new(&path).sort_by_file_name() {
                    let entry = entry?;
                    ensure!(
                        !entry.file_type().is_symlink(),
                        "native configuration must not contain symlinks"
                    );
                    if entry.file_type().is_file() {
                        total += entry.metadata()?.len();
                        ensure!(
                            total <= 64 * 1024 * 1024,
                            "native configuration exceeds 64 MiB; supply a dedicated configuration directory"
                        );
                        digest.update(
                            entry
                                .path()
                                .strip_prefix(&path)?
                                .to_string_lossy()
                                .as_bytes(),
                        );
                        digest.update([0]);
                        digest.update(entry.metadata()?.len().to_le_bytes());
                        digest.update(std::fs::read(entry.path())?);
                    }
                }
            }
        }
        // 模型端点、API 类型或凭据变化后也要隔离旧会话，且不在日志中暴露这些内容。
        if let Some(path) = self.native_model_config()? {
            digest.update(b"pi-models\0");
            digest.update(serde_json::to_vec(&path)?);
            digest.update(std::fs::read(path)?);
        }
        Ok(format!("{:x}", digest.finalize()))
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
