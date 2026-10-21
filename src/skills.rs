//! 发现、校验并复制完整技能包；摘要同时覆盖 SKILL.md 和它引用的附属文件。

use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::config::Config;

/// 一个技能包的入口和内容摘要；path 指向包内的 SKILL.md。
#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub digest: String,
}

#[derive(Debug, Clone)]
pub struct SkillSet {
    pub skills: Vec<Skill>,
    pub digest: String,
    pub root: Option<PathBuf>,
}

#[derive(Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
}

impl SkillSet {
    /// 借用配置，只读取源目录；按名称排序后计算摘要，使扫描结果稳定。
    pub fn discover(config: &Config) -> Result<Self> {
        let mut found = BTreeMap::<String, Skill>::new();
        let mut visited = HashSet::new();
        for root in &config.skills.directories {
            for entry in WalkDir::new(root).follow_links(false).sort_by_file_name() {
                let entry = entry?;
                if entry.file_name() != "SKILL.md" || !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path().canonicalize()?;
                if !visited.insert(path.clone()) {
                    continue;
                }
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("read skill {}", path.display()))?;
                let normalized = text.replace("\r\n", "\n");
                let body = normalized
                    .strip_prefix("---\n")
                    .context("SKILL.md requires YAML frontmatter")?;
                let (yaml, _) = body
                    .split_once("\n---")
                    .context("unterminated skill frontmatter")?;
                let fm: Frontmatter = serde_yaml::from_str(yaml)
                    .with_context(|| format!("invalid skill frontmatter: {}", path.display()))?;
                ensure!(
                    !fm.name.is_empty()
                        && fm.name.len() <= 64
                        && fm
                            .name
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                        && !fm.name.starts_with('-')
                        && !fm.name.ends_with('-'),
                    "invalid skill name: {}",
                    fm.name
                );
                ensure!(
                    !fm.description.trim().is_empty(),
                    "skill {} has an empty description",
                    fm.name
                );
                let digest = directory_digest(path.parent().unwrap())?;
                let skill = Skill {
                    name: fm.name.clone(),
                    description: fm.description,
                    path,
                    digest,
                };
                ensure!(
                    found.insert(fm.name.clone(), skill).is_none(),
                    "duplicate skill name: {}",
                    fm.name
                );
            }
        }
        for name in &config.skills.enabled {
            ensure!(found.contains_key(name), "enabled skill not found: {name}");
        }
        let skills: Vec<_> = found
            .into_values()
            .filter(|s| config.skills.enabled.is_empty() || config.skills.enabled.contains(&s.name))
            .collect();
        for capability in &config.capabilities {
            for name in &capability.required_skills {
                ensure!(
                    skills.iter().any(|s| &s.name == name),
                    "capability {} requires unavailable skill {name}",
                    capability.id
                );
            }
        }
        let mut digest = Sha256::new();
        for s in &skills {
            digest.update(s.name.as_bytes());
            digest.update(s.digest.as_bytes());
        }
        Ok(Self {
            skills,
            digest: format!("{:x}", digest.finalize()),
            root: None,
        })
    }

    /// 取得 self 的所有权，把路径改成存储目录中的副本后返回新的技能集合。
    /// 源技能不会被修改；临时目录复制完成后才重命名为正式目录。
    pub fn snapshot(mut self, storage: &Path) -> Result<Self> {
        let root = storage.join("skills").join(&self.digest);
        if !root.exists() {
            std::fs::create_dir_all(storage.join("skills"))?;
            let temp = tempfile::Builder::new()
                .prefix(".tmp-")
                .tempdir_in(storage.join("skills"))?;
            for skill in &self.skills {
                copy_directory(skill.path.parent().unwrap(), &temp.path().join(&skill.name))?;
            }
            std::fs::rename(temp.path(), &root)?;
        }
        for skill in &mut self.skills {
            skill.path = root.join(&skill.name).join("SKILL.md");
            ensure!(
                directory_digest(skill.path.parent().unwrap())? == skill.digest,
                "skill snapshot failed integrity check: {}",
                skill.name
            );
        }
        self.root = Some(root);
        Ok(self)
    }
}

// 固定遍历顺序，并校验符号链接与总大小；不仅对 Markdown 正文计算摘要。
fn directory_digest(root: &Path) -> Result<String> {
    let mut digest = Sha256::new();
    let mut total = 0u64;
    for entry in WalkDir::new(root).sort_by_file_name() {
        let entry = entry?;
        ensure!(
            !entry.file_type().is_symlink(),
            "skill packages must not contain symlinks: {}",
            entry.path().display()
        );
        if !entry.file_type().is_file() {
            continue;
        }
        total += entry.metadata()?.len();
        ensure!(total <= 64 * 1024 * 1024, "skill package exceeds 64 MiB");
        digest.update(
            entry
                .path()
                .strip_prefix(root)?
                .to_string_lossy()
                .as_bytes(),
        );
        digest.update([0]);
        digest.update(entry.metadata()?.len().to_le_bytes());
        digest.update(std::fs::read(entry.path())?);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            digest.update(entry.metadata()?.permissions().mode().to_le_bytes());
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// 递归复制包内容并保留文件权限；拒绝符号链接，以及把目标放入源目录的递归复制。
pub fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)?;
    ensure!(
        !destination
            .canonicalize()?
            .starts_with(source.canonicalize()?),
        "snapshot destination must be outside its source directory"
    );
    for entry in WalkDir::new(source).sort_by_file_name() {
        let entry = entry?;
        ensure!(
            !entry.file_type().is_symlink(),
            "configuration and skill packages must not contain symlinks"
        );
        let target = destination.join(entry.path().strip_prefix(source)?);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(target)?;
        } else if entry.file_type().is_file() {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
