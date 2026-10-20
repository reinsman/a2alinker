use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Pi,
    #[default]
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub server: ServerConfig,
    pub identity: IdentityConfig,
    pub agent: AgentConfig,
    pub skills: SkillsConfig,
    pub runtime: RuntimeConfig,
    pub storage: StorageConfig,
    pub auth: AuthConfig,
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub public_url: Option<String>,
    pub max_request_bytes: usize,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".parse().unwrap(),
            public_url: None,
            max_request_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct IdentityConfig {
    pub id: String,
    pub name: String,
    pub description: String,
}
impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            id: "agent".into(),
            name: "A2A Linker".into(),
            description: "A local coding agent exposed through A2A".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub kind: AgentKind,
    pub executable: Option<PathBuf>,
    pub native_config: Option<PathBuf>,
    pub workspace: PathBuf,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub inherit_auth: bool,
    pub allow_extensions: bool,
    pub sandbox: String,
    pub approval_policy: String,
}
impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            kind: AgentKind::Codex,
            executable: None,
            native_config: None,
            workspace: ".".into(),
            model: None,
            provider: None,
            args: vec![],
            env: BTreeMap::new(),
            inherit_auth: true,
            allow_extensions: false,
            sandbox: "workspace-write".into(),
            approval_policy: "on-request".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsConfig {
    pub directories: Vec<PathBuf>,
    pub enabled: Vec<String>,
    pub inherit_user: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    pub max_concurrent_tasks: usize,
    pub queue_capacity: usize,
    pub max_sessions: usize,
    pub task_timeout_seconds: u64,
    pub session_idle_timeout_seconds: u64,
    pub startup_timeout_seconds: u64,
    pub cancel_timeout_seconds: u64,
    pub max_protocol_record_bytes: usize,
    pub max_output_bytes: usize,
}
impl Default for RuntimeConfig {
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
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    pub directory: PathBuf,
}
impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            directory: "./data/agent".into(),
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
pub struct AuthConfig {
    pub mode: AuthMode,
    pub token_env: String,
}
impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            mode: AuthMode::None,
            token_env: "A2ALINKER_TOKEN".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub examples: Vec<String>,
    #[serde(default)]
    pub required_skills: Vec<String>,
}

impl Config {
    pub fn load(file: Option<&Path>) -> Result<Self> {
        let mut cfg: Self = match file {
            Some(path) => toml::from_str(
                &std::fs::read_to_string(path)
                    .with_context(|| format!("read configuration {}", path.display()))?,
            )
            .with_context(|| format!("parse configuration {}", path.display()))?,
            None => Self {
                schema_version: 1,
                ..Self::default()
            },
        };
        let cwd = std::env::current_dir()?;
        let base = file.and_then(Path::parent).unwrap_or(&cwd);
        let base = if base.is_absolute() {
            base.to_path_buf()
        } else {
            cwd.join(base)
        };
        cfg.resolve_paths(&base)?;
        Ok(cfg)
    }

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

    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "schema_version must be 1");
        ensure!(
            !self.identity.id.trim().is_empty() && !self.identity.name.trim().is_empty(),
            "identity id and name must not be empty"
        );
        ensure!(
            self.agent.workspace.is_dir(),
            "workspace is not a directory: {}",
            self.agent.workspace.display()
        );
        if let Some(p) = &self.agent.native_config {
            ensure!(
                p.exists(),
                "native configuration does not exist: {}",
                p.display()
            );
        }
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
            ["read-only", "workspace-write"].contains(&self.agent.sandbox.as_str()),
            "sandbox must be read-only or workspace-write"
        );
        ensure!(
            self.agent.kind != AgentKind::Pi || self.agent.sandbox != "read-only",
            "pi has no native read-only sandbox; configure tool restrictions or an OS/container boundary explicitly"
        );
        ensure!(
            ["on-request", "never", "untrusted"].contains(&self.agent.approval_policy.as_str()),
            "unsupported approval_policy"
        );
        let mut ids = std::collections::HashSet::new();
        for cap in &self.capabilities {
            ensure!(
                !cap.id.is_empty() && !cap.name.is_empty() && !cap.description.is_empty(),
                "capability fields must not be empty"
            );
            ensure!(ids.insert(&cap.id), "duplicate capability id: {}", cap.id);
        }
        Ok(())
    }

    pub fn executable(&self) -> PathBuf {
        self.agent
            .executable
            .clone()
            .unwrap_or_else(|| self.agent.kind.as_str().into())
    }

    pub fn public_url(&self) -> String {
        self.server
            .public_url
            .clone()
            .unwrap_or_else(|| format!("http://{}", self.server.listen))
            .trim_end_matches('/')
            .into()
    }

    pub fn fingerprint(&self, skill_digest: &str) -> Result<String> {
        // Only execution settings affect session compatibility; credentials are hashed, never logged.
        let content = serde_json::to_vec(&(&self.agent, &self.skills, skill_digest))?;
        let mut digest = Sha256::new();
        digest.update(content);
        if let Some(path) = &self.agent.native_config {
            if path.is_file() {
                digest.update(std::fs::read(path)?);
            } else {
                let mut total = 0u64;
                for entry in walkdir::WalkDir::new(path).sort_by_file_name() {
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
                                .strip_prefix(path)?
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
